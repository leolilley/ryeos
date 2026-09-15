//! Bounded ingress adaptation onto one protected daemon channel.
//!
//! CLI and structured protocol requests share framing, slots and pending
//! responses. None is a grant, executor or durable operation ledger.
//! Caller prefixes are never accepted as already-normalized identities.

use std::collections::{HashMap, hash_map::Entry};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use ryeos_runtime::workload_client::{
    WORKLOAD_CLIENT_PROTOCOL, WorkloadClientBootFrame, WorkloadClientDispatchFrame,
    WorkloadClientIngress, WorkloadClientOutcome, WorkloadClientReadyFrame,
    WorkloadClientRequestFrame, WorkloadClientResponseFrame, WorkloadInvocationSource,
};

type PendingResponses = Arc<Mutex<HashMap<String, SyncSender<WorkloadClientResponseFrame>>>>;

pub struct RunningWorkloadClientBroker {
    endpoint: Option<String>,
    channel: WorkloadClientChannel,
    accept_thread: Option<thread::JoinHandle<()>>,
}

impl RunningWorkloadClientBroker {
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }
    pub fn channel(&self) -> WorkloadClientChannel {
        self.channel.clone()
    }
}

impl Drop for RunningWorkloadClientBroker {
    fn drop(&mut self) {
        self.channel.stopping.store(true, Ordering::Release);
        let _ = self.channel.interrupt.shutdown();
        if let Some(endpoint) = &self.endpoint {
            // Wake accept. Lillux refuses the runtime itself as an invoking peer.
            let _ = lillux::LocalDuplexStream::connect(Path::new(endpoint));
        }
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
    }
}

struct SlotPool(Mutex<usize>);
impl SlotPool {
    fn try_acquire(self: &Arc<Self>) -> Option<SlotGuard> {
        let mut available = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if *available == 0 {
            return None;
        }
        *available -= 1;
        Some(SlotGuard(Arc::clone(self)))
    }
}
struct SlotGuard(Arc<SlotPool>);
impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut available = self.0.0.lock().unwrap_or_else(|error| error.into_inner());
        *available += 1;
    }
}

/// A transport handle for trusted bridge code, never delivered to the workload.
#[derive(Clone)]
pub struct WorkloadClientChannel {
    writer: Arc<Mutex<lillux::InheritedDuplexChannel>>,
    pending: PendingResponses,
    slots: Arc<SlotPool>,
    stopping: Arc<AtomicBool>,
    max_request_bytes: usize,
    ingresses: Vec<WorkloadClientIngress>,
    execution_presentation: serde_json::Value,
    deadline: lillux::time::MonotonicDeadline,
    interrupt: Arc<lillux::InheritedDuplexChannel>,
}

impl WorkloadClientChannel {
    pub fn execution_presentation(&self) -> &serde_json::Value {
        &self.execution_presentation
    }
    pub fn admits(&self, ingress: WorkloadClientIngress) -> bool {
        self.ingresses.contains(&ingress)
    }

    /// Do not block the App Server event loop on a child or full protected
    /// pipe. The common slot ceiling bounds these invocation tasks.
    pub fn submit(
        &self,
        source: WorkloadInvocationSource,
        request: WorkloadClientRequestFrame,
    ) -> Result<Receiver<WorkloadClientResponseFrame>> {
        request.validate()?;
        source.request_id()?;
        if !self.admits(source.ingress()) || self.stopping.load(Ordering::Acquire) {
            bail!("workload ingress is absent or closed");
        }
        let slot = self
            .slots
            .try_acquire()
            .ok_or_else(|| anyhow!("workload ingress is full"))?;
        let (sender, receiver) = sync_channel(1);
        let channel = self.clone();
        thread::Builder::new()
            .name("ryeos-workload-invocation".to_owned())
            .spawn(move || {
                let _slot = slot;
                let _ = sender.send(channel.exchange(source, request));
            })
            .context("start bounded workload invocation")?;
        Ok(receiver)
    }

    fn exchange(
        &self,
        source: WorkloadInvocationSource,
        mut request: WorkloadClientRequestFrame,
    ) -> WorkloadClientResponseFrame {
        let external_id = request.request_id.clone();
        let outcome = (|| -> Result<WorkloadClientResponseFrame> {
            request.validate()?;
            if !self.admits(source.ingress()) || self.stopping.load(Ordering::Acquire) {
                bail!("workload ingress is absent or closed");
            }
            request.request_id = source.request_id()?;
            let id = request.request_id.clone();
            let dispatch = WorkloadClientDispatchFrame { source, request };
            dispatch.validate()?;
            if serde_json::to_vec(&dispatch)?.len() > self.max_request_bytes {
                bail!("workload request exceeds admitted byte limit");
            }
            let (sender, receiver) = sync_channel(1);
            {
                let mut pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                match pending.entry(id.clone()) {
                    Entry::Vacant(entry) => {
                        entry.insert(sender);
                    }
                    Entry::Occupied(_) => bail!("workload occurrence is already in flight"),
                }
            }
            let sent = self
                .writer
                .lock()
                .map_err(|_| anyhow!("workload channel writer is poisoned"))
                .and_then(|mut writer| {
                    ryeos_runtime::workload_client::write_frame_bounded(
                        &mut writer.with_deadline(self.deadline),
                        &dispatch,
                        self.max_request_bytes,
                    )
                });
            if sent.is_err() {
                self.pending
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&id);
                // Partial write is not proof of no contact; retire the framing
                // stream and never retry this call through another ingress.
                self.stopping.store(true, Ordering::Release);
                let _ = self.interrupt.shutdown();
                return Ok(unknown_response(&external_id));
            }
            Ok(match receiver.recv_timeout(self.deadline.remaining()) {
                Ok(mut response) => {
                    response.request_id = external_id.clone();
                    response
                }
                Err(_) => {
                    self.stopping.store(true, Ordering::Release);
                    let _ = self.interrupt.shutdown();
                    unknown_response(&external_id)
                }
            })
        })();
        outcome.unwrap_or_else(|error| {
            failure_response(&external_id, "broker-refused", &error.to_string())
        })
    }
}

pub fn start(
    mut daemon_channel: lillux::InheritedDuplexChannel,
    supported_ingresses: &[WorkloadClientIngress],
) -> Result<RunningWorkloadClientBroker> {
    let boot: WorkloadClientBootFrame = ryeos_runtime::workload_client::read_frame_bounded(
        &mut daemon_channel,
        ryeos_runtime::workload_client::MAX_WORKLOAD_CLIENT_CONTROL_FRAME_BYTES,
    )
    .context("read workload boot contract")?;
    boot.validate()?;
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        boot.max_lifetime_seconds,
    ));
    if boot
        .ingresses
        .iter()
        .any(|ingress| !supported_ingresses.contains(ingress))
    {
        bail!("selected workload ingress is not admitted by the compiled profile");
    }
    let listener = if boot.ingresses.contains(&WorkloadClientIngress::Cli) {
        Some(
            lillux::OwnerPrivateLocalDuplexListener::bind_isolated_runtime(
                ryeos_runtime::workload_client::WORKLOAD_CLIENT_BROKER_DIRECTORY_NAME,
                "w",
            )?,
        )
    } else {
        None
    };
    let endpoint = listener
        .as_ref()
        .map(|listener| {
            listener
                .endpoint()
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("workload endpoint is not UTF-8"))
        })
        .transpose()?;
    let ready = WorkloadClientReadyFrame {
        protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
        grant_digest: boot.grant_digest.clone(),
    };
    ryeos_runtime::workload_client::write_frame_bounded(
        &mut daemon_channel,
        &ready,
        ryeos_runtime::workload_client::MAX_WORKLOAD_CLIENT_CONTROL_FRAME_BYTES,
    )?;
    let reader = daemon_channel.try_clone()?;
    let interrupt = Arc::new(daemon_channel.try_clone()?);
    let channel = WorkloadClientChannel {
        writer: Arc::new(Mutex::new(daemon_channel)),
        pending: Arc::new(Mutex::new(HashMap::new())),
        slots: Arc::new(SlotPool(Mutex::new(usize::from(boot.max_in_flight)))),
        stopping: Arc::new(AtomicBool::new(false)),
        max_request_bytes: boot.max_request_bytes as usize,
        ingresses: boot.ingresses,
        execution_presentation: boot.execution_presentation,
        deadline,
        interrupt: Arc::clone(&interrupt),
    };
    let response_pending = Arc::clone(&channel.pending);
    let response_stopping = Arc::clone(&channel.stopping);
    thread::Builder::new()
        .name("ryeos-workload-responses".to_owned())
        .spawn(move || {
            read_daemon_responses(reader, response_pending, response_stopping, deadline);
            let _ = interrupt.shutdown();
        })?;
    let accept_channel = channel.clone();
    let accept_thread = listener
        .map(|listener| {
            thread::Builder::new()
                .name("ryeos-workload-accept".to_owned())
                .spawn(move || {
                    loop {
                        let stream = match listener.accept_isolated_descendant() {
                            Ok(stream) => stream,
                            Err(_) if accept_channel.stopping.load(Ordering::Acquire) => return,
                            Err(_) => continue,
                        };
                        if accept_channel.stopping.load(Ordering::Acquire) {
                            return;
                        }
                        // An idle listener must not reserve the shared slot and starve
                        // protocol ingress. Acquire after exact peer admission instead.
                        let Some(slot) = accept_channel.slots.try_acquire() else {
                            continue;
                        };
                        let channel = accept_channel.clone();
                        if thread::Builder::new()
                            .name("ryeos-workload-cli".to_owned())
                            .spawn(move || {
                                let _slot = slot;
                                handle_local_invocation(stream, channel);
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                })
        })
        .transpose()?;
    Ok(RunningWorkloadClientBroker {
        endpoint,
        channel,
        accept_thread,
    })
}

fn handle_local_invocation(mut stream: lillux::LocalDuplexStream, channel: WorkloadClientChannel) {
    let mut stream = stream.with_deadline(channel.deadline);
    let request =
        ryeos_runtime::workload_client::read_frame_bounded(&mut stream, channel.max_request_bytes);
    let response = match request {
        Ok(request) => {
            let request: WorkloadClientRequestFrame = request;
            let source = WorkloadInvocationSource::Cli {
                external_request_id: request.request_id.clone(),
            };
            channel.exchange(source, request)
        }
        Err(_) => failure_response(
            "invalid-request",
            "broker-refused",
            "invalid workload request",
        ),
    };
    let _ = ryeos_runtime::workload_client::write_frame(&mut stream, &response);
}

fn read_daemon_responses(
    mut reader: lillux::InheritedDuplexChannel,
    pending: PendingResponses,
    stopping: Arc<AtomicBool>,
    deadline: lillux::time::MonotonicDeadline,
) {
    let mut reader = reader.with_deadline(deadline);
    loop {
        let response: WorkloadClientResponseFrame =
            match ryeos_runtime::workload_client::read_frame(&mut reader) {
                Ok(response) => response,
                Err(_) => break,
            };
        if response.validate().is_err() {
            break;
        }
        let sender = pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&response.request_id);
        let Some(sender) = sender else {
            break;
        };
        let _ = sender.send(response);
    }
    stopping.store(true, Ordering::Release);
    let abandoned = std::mem::take(&mut *pending.lock().unwrap_or_else(|error| error.into_inner()));
    for (request_id, sender) in abandoned {
        let _ = sender.send(unknown_response(&request_id));
    }
}

fn unknown_response(request_id: &str) -> WorkloadClientResponseFrame {
    failure_response(
        request_id,
        ryeos_runtime::callback::RUNTIME_ACTION_OUTCOME_UNKNOWN_CODE,
        "protected channel closed after possible execution contact; do not retry as a new occurrence",
    )
}

fn failure_response(request_id: &str, code: &str, message: &str) -> WorkloadClientResponseFrame {
    WorkloadClientResponseFrame {
        protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
        request_id: if ryeos_runtime::workload_client::validate_request_id(request_id).is_ok() {
            request_id.to_owned()
        } else {
            "invalid-request".to_owned()
        },
        outcome: WorkloadClientOutcome::Failed {
            code: code.to_owned(),
            message: ryeos_runtime::workload_client::bounded_error_message(message),
            retryable: false,
        },
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use lillux::time::{Duration, MonotonicDeadline};
    use ryeos_runtime::workload_client::*;
    use serde_json::json;

    const TEST_CHANNEL: &str = "RYEOS_TEST_DUAL_CHANNEL";
    const TEST_ROLE: &str = "RYEOS_TEST_DUAL_ROLE";
    const TEST_ENTRY: &str = "workload_client_broker::tests::native_dual_ingress";

    fn request() -> WorkloadClientRequestFrame {
        WorkloadClientRequestFrame {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            request_id: "same-caller-id".to_owned(),
            operation: WorkloadClientOperation::Execute(WorkloadClientExecuteRequest {
                item_ref: "tool:fixture/check".to_owned(),
                ref_bindings: Default::default(),
                params: json!({}),
                call: None,
            }),
        }
    }

    #[test]
    #[ignore = "requires unprivileged Linux PID/mount namespaces; no sudo or installed node"]
    fn native_dual_ingress() {
        match std::env::var(TEST_ROLE).ok().as_deref() {
            Some("client") => {
                let endpoint = std::env::var("RYEOS_TEST_DUAL_ENDPOINT").unwrap();
                if let Some(binary) = std::env::var_os("RYEOS_TEST_DUAL_CLIENT_BINARY") {
                    // Explicit second qualification mode exercises the real
                    // restricted CLI executable, not just its wire grammar.
                    let output = std::process::Command::new(binary)
                        .args(["execute", "tool:fixture/check", "--params", "{}"])
                        .env(WORKLOAD_CLIENT_ENDPOINT_ENV, endpoint)
                        .output()
                        .unwrap();
                    assert!(!output.status.success());
                    assert!(
                        String::from_utf8(output.stderr)
                            .unwrap()
                            .contains("test-child-failed")
                    );
                    return;
                }
                let mut stream = lillux::LocalDuplexStream::connect_isolated_runtime_broker(
                    Path::new(&endpoint),
                )
                .unwrap();
                let mut stream =
                    stream.with_deadline(MonotonicDeadline::after(Duration::from_secs(15)));
                write_frame(&mut stream, &request()).unwrap();
                let response: WorkloadClientResponseFrame = read_frame(&mut stream).unwrap();
                response.validate().unwrap();
                assert_eq!(response.request_id, "same-caller-id");
                assert!(!response.outcome.succeeded());
            }
            Some("broker") => {
                // SAFETY: this test's parent transfers the unique endpoint
                // through Lillux across the namespace launcher before exec.
                let channel =
                    unsafe { lillux::take_inherited_duplex_channel_from_env(TEST_CHANNEL) }
                        .unwrap();
                let broker = start(
                    channel,
                    &[
                        WorkloadClientIngress::Cli,
                        WorkloadClientIngress::StructuredSession,
                    ],
                )
                .unwrap();
                let source = WorkloadInvocationSource::StructuredSession {
                    upstream_session_id: "session".to_owned(),
                    operation_id: "turn".to_owned(),
                    call_id: "same-caller-id".to_owned(),
                };
                // The idle CLI listener must not reserve the only shared slot.
                let response = broker.channel.submit(source.clone(), request()).unwrap();
                assert!(broker.channel.submit(source, request()).is_err());
                assert!(
                    !response
                        .recv_timeout(Duration::from_secs(15))
                        .unwrap()
                        .outcome
                        .succeeded()
                );
                let deadline = MonotonicDeadline::after(Duration::from_secs(2));
                while *broker.channel.slots.0.lock().unwrap() == 0 {
                    assert!(!deadline.has_elapsed());
                    lillux::time::sleep(Duration::from_millis(1));
                }
                let status = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--ignored", "--exact", TEST_ENTRY, "--nocapture"])
                    .env(TEST_ROLE, "client")
                    .env("RYEOS_TEST_DUAL_ENDPOINT", broker.endpoint().unwrap())
                    .status()
                    .unwrap();
                assert!(status.success());
            }
            None => {
                let (mut channel, child_channel) = lillux::inherited_duplex_channel_pair().unwrap();
                // External test orchestration only: production namespaces and
                // all peer/descriptor authority remain Lillux-owned. The tmpfs
                // is mounted only inside this fresh unprivileged namespace.
                let mut command = std::process::Command::new("unshare");
                command
                    .args([
                        "--user",
                        "--map-root-user",
                        "--mount",
                        "--pid",
                        "--fork",
                        "--mount-proc",
                        "/bin/sh",
                        "-c",
                        "mount -t tmpfs -o mode=1777 tmpfs /tmp && exec \"$@\"",
                        "probe",
                    ])
                    .arg(std::env::current_exe().unwrap())
                    .args(["--ignored", "--exact", TEST_ENTRY, "--nocapture"])
                    .env(TEST_ROLE, "broker");
                child_channel
                    .bind_to_command(&mut command, TEST_CHANNEL)
                    .unwrap();
                let mut child = command.spawn().unwrap();
                drop(command);
                let mut channel =
                    channel.with_deadline(MonotonicDeadline::after(Duration::from_secs(20)));
                write_frame(
                    &mut channel,
                    &WorkloadClientBootFrame {
                        protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
                        grant_digest: "a".repeat(64),
                        ingresses: vec![
                            WorkloadClientIngress::Cli,
                            WorkloadClientIngress::StructuredSession,
                        ],
                        execution_presentation: json!([{}]),
                        max_lifetime_seconds: 20,
                        max_in_flight: 1,
                        max_request_bytes: 4096,
                    },
                )
                .unwrap();
                let ready: WorkloadClientReadyFrame = read_frame(&mut channel).unwrap();
                ready.validate().unwrap();
                let mut ids = std::collections::HashSet::new();
                for ingress in [
                    WorkloadClientIngress::StructuredSession,
                    WorkloadClientIngress::Cli,
                ] {
                    let frame: WorkloadClientDispatchFrame = read_frame(&mut channel).unwrap();
                    frame.validate().unwrap();
                    assert_eq!(frame.source.ingress(), ingress);
                    assert!(ids.insert(frame.request.request_id.clone()));
                    write_frame(
                        &mut channel,
                        &failure_response(
                            &frame.request.request_id,
                            "test-child-failed",
                            "deliberate test failure",
                        ),
                    )
                    .unwrap();
                }
                assert!(child.wait().unwrap().success());
            }
            Some(other) => panic!("unknown test role {other}"),
        }
    }
}
