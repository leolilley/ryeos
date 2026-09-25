//! Trusted controller-side stdio connector for one admitted external candidate.
//!
//! Codex starts this exact binary as a command-backed environment. It does not
//! execute commands itself: stdin/stdout are relayed to the daemon's durable
//! occurrence channel through one owner-private authenticated Unix stream.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_state::external_execution::MAX_CHUNK_BYTES;
use ryeos_state::external_execution::connector::{
    ExternalConnectorClientFrame, ExternalConnectorHello, ExternalConnectorServerFrame,
    read_external_connector_server_frame, write_external_connector_client_frame,
    write_external_connector_hello,
};

const ENDPOINT_ENV: &str = "RYEOS_EXTERNAL_CONNECTOR_ENDPOINT";
const PLACEMENT_ENV: &str = "RYEOS_EXTERNAL_CONNECTOR_PLACEMENT";
const BINDING_ENV: &str = "RYEOS_EXTERNAL_CONNECTOR_BINDING";
const CAPABILITY_ENV: &str = "RYEOS_EXTERNAL_CONNECTOR_CAPABILITY";
const LOCAL_IO_TIMEOUT: lillux::time::Duration = lillux::time::Duration::from_secs(30);

enum InputRelayPhase {
    Reading,
    Writing { local_sequence: u64 },
    AwaitingAcknowledgement { local_sequence: u64 },
    Closing,
    Closed,
    OutputClosed,
    Failed(Option<anyhow::Error>),
}

struct InputRelayStatus {
    state: Mutex<InputRelayState>,
    changed: lillux::task::HostCondition,
}

struct InputRelayState {
    phase: InputRelayPhase,
    acknowledged_sequence: u64,
    stop_requested: bool,
    #[cfg(test)]
    close_wait_entered: bool,
}

impl InputRelayStatus {
    fn new() -> Self {
        Self {
            state: Mutex::new(InputRelayState {
                phase: InputRelayPhase::Reading,
                acknowledged_sequence: 0,
                stop_requested: false,
                #[cfg(test)]
                close_wait_entered: false,
            }),
            changed: lillux::task::HostCondition::default(),
        }
    }

    fn set(&self, phase: InputRelayPhase) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .phase = phase;
        self.changed.notify_all();
    }

    fn fail(&self, error: anyhow::Error) {
        self.set(InputRelayPhase::Failed(Some(error)));
    }

    /// Atomically acquire permission for one next input frame. Once output EOF
    /// seals the relay, a stdin read that completes later cannot begin a write.
    fn begin_write(&self, local_sequence: u64, closing: bool) -> Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.stop_requested {
            return Ok(false);
        }
        match &state.phase {
            InputRelayPhase::Reading => {
                state.phase = if closing {
                    InputRelayPhase::Closing
                } else {
                    InputRelayPhase::Writing { local_sequence }
                };
                self.changed.notify_all();
                Ok(true)
            }
            InputRelayPhase::OutputClosed => Ok(false),
            InputRelayPhase::Failed(_) => {
                bail!("external connector input relay is already failed")
            }
            _ => bail!("external connector attempted overlapping input writes"),
        }
    }

    fn take_failure(&self) -> Option<anyhow::Error> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &mut state.phase {
            InputRelayPhase::Failed(error) => error.take(),
            _ => None,
        }
    }

    fn output_closed(&self) -> bool {
        matches!(
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .phase,
            InputRelayPhase::OutputClosed
        )
    }

    fn acknowledge(&self, local_sequence: u64) -> Result<()> {
        let deadline = lillux::time::MonotonicDeadline::after(LOCAL_IO_TIMEOUT);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            match &mut state.phase {
                InputRelayPhase::Writing {
                    local_sequence: writing,
                } if *writing == local_sequence => {
                    ensure!(
                        !deadline.has_elapsed(),
                        "external connector input write did not settle before acknowledgement"
                    );
                    state = self
                        .changed
                        .wait_until(state, deadline)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                InputRelayPhase::AwaitingAcknowledgement {
                    local_sequence: expected,
                } if *expected == local_sequence => {
                    state.acknowledged_sequence = local_sequence;
                    state.phase = InputRelayPhase::Reading;
                    self.changed.notify_all();
                    return Ok(());
                }
                InputRelayPhase::Failed(error) => {
                    return Err(error.take().unwrap_or_else(|| {
                        anyhow::anyhow!("external connector input relay failed")
                    }));
                }
                _ => bail!("external connector input acknowledgement changed sequence"),
            }
        }
    }

    fn require_clean_output_eof(&self) -> Result<()> {
        self.require_clean_output_eof_until(lillux::time::MonotonicDeadline::after(
            LOCAL_IO_TIMEOUT,
        ))
    }

    fn require_clean_output_eof_until(
        &self,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            match &mut state.phase {
                InputRelayPhase::Reading | InputRelayPhase::Closed => {
                    state.phase = InputRelayPhase::OutputClosed;
                    self.changed.notify_all();
                    return Ok(());
                }
                InputRelayPhase::OutputClosed => return Ok(()),
                InputRelayPhase::Failed(error) => {
                    return Err(error.take().unwrap_or_else(|| {
                        anyhow::anyhow!("external connector input relay failed")
                    }));
                }
                InputRelayPhase::Closing => {
                    // Peer EOF can overtake the writer's local bookkeeping after
                    // InputClosed was delivered. Wait for actual write settlement;
                    // Closing itself never proves delivery or permits success.
                    ensure!(
                        !deadline.has_elapsed(),
                        "external connector input close did not settle"
                    );
                    #[cfg(test)]
                    {
                        // Test-only observation, never a release or success
                        // control: prove EOF really waited before bookkeeping.
                        state.close_wait_entered = true;
                        self.changed.notify_all();
                    }
                    state = self
                        .changed
                        .wait_until(state, deadline)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                InputRelayPhase::Writing { .. }
                | InputRelayPhase::AwaitingAcknowledgement { .. } => {
                    bail!("external connector output closed with uncertain input delivery")
                }
            }
        }
    }

    fn stop_waiting(&self) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stop_requested = true;
        self.changed.notify_all();
    }

    fn wait_for_acknowledgement(
        &self,
        sequence: u64,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            ensure!(
                !deadline.has_elapsed(),
                "external connector input acknowledgement timed out"
            );
            // Preserve an acknowledgement even when output EOF or cleanup has
            // subsequently sealed the relay before this waiter runs again.
            if state.acknowledged_sequence == sequence {
                return Ok(());
            }
            ensure!(
                !state.stop_requested,
                "external connector acknowledgement wait interrupted"
            );
            ensure!(
                matches!(&state.phase,
                InputRelayPhase::AwaitingAcknowledgement { local_sequence } if *local_sequence == sequence),
                "external connector input acknowledgement changed sequence"
            );
            state = self
                .changed
                .wait_until(state, deadline)
                .unwrap_or_else(|p| p.into_inner());
        }
    }
}

fn main() {
    if let Err(error) = run() {
        // Wire types never include secret values in validation messages. Keep
        // this as the sole diagnostic boundary; protocol content is not logged.
        eprintln!("ryeos-external-candidate-connector: {error:#}");
        std::process::exit(126);
    }
}

fn run() -> Result<()> {
    ensure!(
        std::env::args_os().count() == 1,
        "external connector accepts no command arguments"
    );
    let endpoint = PathBuf::from(required_env(ENDPOINT_ENV)?);
    let placement_thread_id = required_env(PLACEMENT_ENV)?;
    let execution_binding_hash = required_env(BINDING_ENV)?;
    let capability = required_secret_env(CAPABILITY_ENV)?;
    let hello = ExternalConnectorHello::new(
        placement_thread_id.clone(),
        execution_binding_hash.clone(),
        capability,
    )?;

    // SAFETY: exclusive executable startup, before Rust stdin/stdout use or
    // host tasks. Codex supplies distinct piped endpoints and retains only
    // their opposite ends. Neither inherited endpoint has another owner here.
    let pipes = unsafe {
        lillux::inherited_pipes::InheritedPipePair::take_inherited_pipes(
            0,
            1,
            MAX_CHUNK_BYTES,
            lillux::time::MonotonicDeadline::after(LOCAL_IO_TIMEOUT),
        )
    }?;
    let (input, mut output, pipe_interrupt) = pipes.split();

    let mut stream = lillux::LocalDuplexStream::connect(&endpoint)
        .context("connect protected external candidate controller")?;
    write_external_connector_hello(
        &mut stream.with_deadline(lillux::time::MonotonicDeadline::after(LOCAL_IO_TIMEOUT)),
        &hello,
    )?;
    let first = read_external_connector_server_frame(
        &mut stream.with_deadline(lillux::time::MonotonicDeadline::after(LOCAL_IO_TIMEOUT)),
    )?;
    let ExternalConnectorServerFrame::Ready {
        placement_thread_id: ready_placement,
        execution_binding_hash: ready_binding,
    } = first
    else {
        bail!("external connector was not authenticated")
    };
    ensure!(
        ready_placement == placement_thread_id && ready_binding == execution_binding_hash,
        "external connector readiness changed its admitted execution"
    );

    let writer = stream.try_clone()?;
    let interrupt = stream.try_clone()?;
    let input_status = Arc::new(InputRelayStatus::new());
    let relay_status = Arc::clone(&input_status);
    let relay_pipe_interrupt = pipe_interrupt.clone();
    let input_thread = lillux::task::spawn_host_task("external-connector-input", move || {
        if let Err(error) = relay_input(input, writer, &relay_status) {
            relay_status.fail(error);
            let _ = relay_pipe_interrupt.interrupt();
            let _ = interrupt.shutdown();
        }
    })?;

    let result = (|| -> Result<()> {
        loop {
            if let Some(error) = input_status.take_failure() {
                return Err(error);
            }
            match read_external_connector_server_frame(&mut stream)? {
                ExternalConnectorServerFrame::Ready { .. } => {
                    bail!("external connector received duplicate readiness");
                }
                ExternalConnectorServerFrame::InputApplied { local_sequence, .. } => {
                    input_status.acknowledge(local_sequence)?;
                }
                frame @ ExternalConnectorServerFrame::ProtocolBytes { .. } => {
                    let bytes = frame
                        .protocol_bytes()?
                        .context("external connector output frame lost its bytes")?;
                    output.write_all(
                        &bytes,
                        lillux::time::MonotonicDeadline::after(LOCAL_IO_TIMEOUT),
                    )?;
                }
                ExternalConnectorServerFrame::ProtocolEof { .. } => {
                    input_status.require_clean_output_eof()?;
                    return Ok(());
                }
                ExternalConnectorServerFrame::Fault { .. } => {
                    bail!("external connector controller reported a closed fault");
                }
            }
        }
    })();
    // Sticky pipe interruption wakes an idle stdin owner even when Codex still
    // holds its writer open. Socket shutdown interrupts an in-flight send;
    // notifying retained relay state releases an acknowledgement wait. Join is
    // required on every outcome; neither EOF nor detach establishes settlement.
    let pipe_shutdown = pipe_interrupt.interrupt();
    let _ = stream.shutdown();
    input_status.stop_waiting();
    input_thread
        .join()
        .map_err(|_| anyhow::anyhow!("external connector input relay panicked"))?;
    result?;
    pipe_shutdown?;
    if let Some(error) = input_status.take_failure() {
        return Err(error);
    }
    Ok(())
}

fn relay_input(
    mut input: lillux::inherited_pipes::InheritedPipeInput,
    mut stream: lillux::LocalDuplexStream,
    status: &InputRelayStatus,
) -> Result<()> {
    let mut buffer = vec![0_u8; MAX_CHUNK_BYTES];
    let mut local_sequence = 0_u64;
    loop {
        let read = match input.read_chunk(&mut buffer, None) {
            Ok(count) => count,
            Err(error)
                if error.kind() == std::io::ErrorKind::Interrupted && status.output_closed() =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        local_sequence = local_sequence
            .checked_add(1)
            .context("external connector input sequence overflow")?;
        if !status.begin_write(local_sequence, read == 0)? {
            return Ok(());
        }
        let frame = if read == 0 {
            ExternalConnectorClientFrame::InputClosed { local_sequence }
        } else {
            ExternalConnectorClientFrame::ProtocolBytes {
                local_sequence,
                bytes_base64: STANDARD.encode(&buffer[..read]),
            }
        };
        let delivery_deadline = lillux::time::MonotonicDeadline::after(LOCAL_IO_TIMEOUT);
        write_external_connector_client_frame(
            &mut stream.with_deadline(delivery_deadline),
            &frame,
        )?;
        if read == 0 {
            status.set(InputRelayPhase::Closed);
            return Ok(());
        }
        status.set(InputRelayPhase::AwaitingAcknowledgement { local_sequence });
        status.wait_for_acknowledgement(local_sequence, delivery_deadline)?;
    }
}

fn required_env(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("missing {name}"))?;
    ensure!(
        !value.is_empty() && value.len() <= 4096,
        "external connector environment value exceeds its bound"
    );
    Ok(value)
}

fn required_secret_env(name: &str) -> Result<String> {
    let value = required_env(name)?;
    // SAFETY: this connector has not started any threads, and none of its
    // dependencies starts foreign threads before `run`. Remove the secret
    // before connecting or spawning the input relay so descendants cannot
    // inherit the one-use local capability.
    unsafe { std::env::remove_var(name) };
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_output_eof_atomically_fences_later_input() {
        let status = InputRelayStatus::new();
        status.require_clean_output_eof().unwrap();
        assert!(status.output_closed());
        assert!(!status.begin_write(1, false).unwrap());
        assert!(!status.begin_write(2, true).unwrap());
    }

    #[test]
    fn exact_acknowledgement_allows_eof_but_not_late_input() {
        let status = InputRelayStatus::new();
        assert!(status.begin_write(1, false).unwrap());
        assert!(status.require_clean_output_eof().is_err());
        status.set(InputRelayPhase::AwaitingAcknowledgement { local_sequence: 1 });
        assert!(status.acknowledge(2).is_err());
        assert!(status.require_clean_output_eof().is_err());
        status.acknowledge(1).unwrap();
        status.require_clean_output_eof().unwrap();
        assert!(!status.begin_write(2, false).unwrap());
        assert!(status.acknowledge(1).is_err());
    }

    #[test]
    fn closing_or_failed_input_cannot_be_reported_as_clean_output_eof() {
        let status = InputRelayStatus::new();
        assert!(status.begin_write(1, true).unwrap());
        assert!(
            status
                .require_clean_output_eof_until(lillux::time::MonotonicDeadline::after(
                    lillux::time::Duration::ZERO
                ))
                .is_err()
        );
        status.set(InputRelayPhase::Closed);
        status.require_clean_output_eof().unwrap();

        let status = InputRelayStatus::new();
        status.fail(anyhow::anyhow!("test relay failed"));
        assert!(!status.output_closed());
        assert!(status.require_clean_output_eof().is_err());
        assert!(status.require_clean_output_eof().is_err());
        assert!(status.begin_write(1, false).is_err());
    }

    #[test]
    fn acknowledgement_wait_expires_and_shutdown_wakes_without_inventing_ack() {
        let status = InputRelayStatus::new();
        assert!(status.begin_write(1, false).unwrap());
        status.set(InputRelayPhase::AwaitingAcknowledgement { local_sequence: 1 });
        let deadline =
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(10));
        assert!(
            status
                .wait_for_acknowledgement(1, deadline)
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        status.stop_waiting();
        assert!(
            status
                .wait_for_acknowledgement(
                    1,
                    lillux::time::MonotonicDeadline::after(LOCAL_IO_TIMEOUT)
                )
                .unwrap_err()
                .to_string()
                .contains("interrupted")
        );
        assert!(!status.begin_write(2, false).unwrap());
    }

    #[test]
    fn already_observed_ack_survives_output_eof_and_cleanup() {
        let status = InputRelayStatus::new();
        assert!(status.begin_write(1, false).unwrap());
        status.set(InputRelayPhase::AwaitingAcknowledgement { local_sequence: 1 });
        status.acknowledge(1).unwrap();
        status.require_clean_output_eof().unwrap();
        status.stop_waiting();
        status
            .wait_for_acknowledgement(1, lillux::time::MonotonicDeadline::after(LOCAL_IO_TIMEOUT))
            .unwrap();
        assert!(
            status
                .wait_for_acknowledgement(
                    1,
                    lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO)
                )
                .is_err()
        );
    }

    #[test]
    fn peer_eof_waits_for_actual_close_bookkeeping_or_failure() {
        for successful_write in [true, false] {
            let status = Arc::new(InputRelayStatus::new());
            assert!(status.begin_write(1, true).unwrap());
            let deadline =
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(3));
            let observer = Arc::clone(&status);
            let task = lillux::task::spawn_host_task("connector-close-order", move || {
                observer.require_clean_output_eof_until(deadline)
            })
            .unwrap();
            let state = status.state.lock().unwrap();
            let state = status
                .changed
                .wait_while_until(state, deadline, |state| !state.close_wait_entered)
                .unwrap();
            assert!(state.close_wait_entered, "EOF never reached the close wait");
            assert!(matches!(state.phase, InputRelayPhase::Closing));
            assert!(
                !task.is_finished(),
                "Closing was accepted before settlement"
            );
            drop(state);
            if successful_write {
                status.set(InputRelayPhase::Closed);
            } else {
                status.fail(anyhow::anyhow!("close write failed"));
            }
            let observed = task
                .join_until(deadline)
                .unwrap_or_else(|_| panic!("close observation did not settle"))
                .unwrap();
            assert_eq!(observed.is_ok(), successful_write);
            assert_eq!(status.output_closed(), successful_write);
        }
    }
}
