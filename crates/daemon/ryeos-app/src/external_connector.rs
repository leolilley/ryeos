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
use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use zeroize::Zeroizing;

use ryeos_external_execution_contract::{
    MAX_PROVIDER_CONFIGURATION_BYTES, ProviderConfigurationRequest, ProviderConnectorExecutable,
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
use crate::runtime_db::external_execution::connector::ExternalConnectorPreparation;
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
// Local observation grace, not permission to abandon remote writer ownership.
const RELAY_RETIREMENT_GRACE: Duration = Duration::from_secs(2);

/// Prepared occurrence-private provider configuration and its live relay.
/// Moving the relay into the persistent-session lifelines retains the listener
/// exactly as long as the local provider process can start or use it.
pub struct PreparedExternalCandidateConnector {
    configuration_link: Option<lillux::EphemeralDescriptorFileLink>,
    descriptors: Vec<lillux::InheritedDescriptorAuthority>,
    mounts: Vec<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>,
    mount_targets: Vec<lillux::EmptyMountTargetReservation>,
    relay: ExternalConnectorRelayLifeline,
}

impl PreparedExternalCandidateConnector {
    pub fn into_parts(
        self,
    ) -> (
        Vec<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>,
        Vec<lillux::InheritedDescriptorAuthority>,
        Vec<Box<dyn Send + Sync>>,
        crate::persistent_session::PersistentSessionCleanupObserver,
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
        (self.mounts, self.descriptors, lifelines, settlement)
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
) -> Result<Zeroizing<String>> {
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
    // The adapter reads an occurrence capability. Its diagnostics are private,
    // including parse errors that may quote request values.
    ensure!(
        result.success,
        "provider configuration adapter failed (exit_code={}, timed_out={}, output_limit_exceeded={}, launcher_refused={}, spawn_refusal={})",
        result.exit_code,
        result.timed_out,
        result.output_limit_exceeded.is_some(),
        result.launcher_refusal.is_some(),
        if result.pid == 0 && result.stderr.starts_with("Failed to spawn:") {
            result.stderr.as_str()
        } else {
            "none"
        },
    );
    let encoded = Zeroizing::new(std::mem::take(&mut result.stdout));
    ensure!(
        !encoded.is_empty() && encoded.len() <= MAX_PROVIDER_CONFIGURATION_BYTES,
        "external connector configuration exceeds its bound"
    );
    Ok(encoded)
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
    let guard = Arc::new(TempDirGuard::new_pinned(parent, name, root));
    Ok((listener, listener_mount, guard))
}

/// Prepare the one local connector only after the remote occurrence has
/// authenticated, reached Ready, and received its exact Release.
pub fn prepare_external_candidate_connector(
    state: &AppState,
    placement: &str,
    state_root: &Path,
) -> Result<PreparedExternalCandidateConnector> {
    ensure!(
        state_root.is_absolute(),
        "external connector provider state root must be absolute"
    );
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("external connector has no retained allocation")?;
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
            configuration.as_bytes(),
        )
        .map_err(anyhow::Error::msg)?;
        let state_directory = lillux::PinnedDirectory::open(state_root)?
            .context("external connector provider state root is missing")?;
        state_directory.require_owner_private_directory()?;
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

#[allow(clippy::too_many_arguments)]
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
    // Publish only the closed category and the authoritative occurrence.
    if error.is_some() {
        tracing::warn!(
            placement = %placement,
            reason,
            "external connector relay terminated"
        );
    }
    closed
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

#[allow(clippy::too_many_arguments)]
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
        let peer_identity = peer.exact_process_identity(None)?;
        stage = "durable_connection";
        ensure!(
            state.state_store.connect_external_connector(
                placement,
                capability_generation,
                capability_hash,
                &peer_identity,
            )?,
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

fn is_disconnect(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

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
