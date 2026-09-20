//! Trusted controller-side stdio connector for one admitted external candidate.
//!
//! Codex starts this exact binary as a command-backed environment. It does not
//! execute commands itself: stdin/stdout are relayed to the daemon's durable
//! occurrence channel through one owner-private authenticated Unix stream.

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

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
const LOCAL_IO_TIMEOUT: Duration = Duration::from_secs(30);

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
    phase: Mutex<InputRelayPhase>,
    changed: Condvar,
}

impl InputRelayStatus {
    fn new() -> Self {
        Self {
            phase: Mutex::new(InputRelayPhase::Reading),
            changed: Condvar::new(),
        }
    }

    fn set(&self, phase: InputRelayPhase) {
        *self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = phase;
        self.changed.notify_all();
    }

    fn fail(&self, error: anyhow::Error) {
        self.set(InputRelayPhase::Failed(Some(error)));
    }

    /// Atomically acquire permission for one next input frame. Once output EOF
    /// seals the relay, a stdin read that completes later cannot begin a write.
    fn begin_write(&self, local_sequence: u64, closing: bool) -> Result<bool> {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &*phase {
            InputRelayPhase::Reading => {
                *phase = if closing {
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
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &mut *phase {
            InputRelayPhase::Failed(error) => error.take(),
            _ => None,
        }
    }

    fn acknowledge(&self, local_sequence: u64) -> Result<()> {
        let deadline = std::time::Instant::now() + LOCAL_IO_TIMEOUT;
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            match &mut *phase {
                InputRelayPhase::Writing {
                    local_sequence: writing,
                } if *writing == local_sequence => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    ensure!(
                        !remaining.is_zero(),
                        "external connector input write did not settle before acknowledgement"
                    );
                    let waited = self
                        .changed
                        .wait_timeout(phase, remaining)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    phase = waited.0;
                }
                InputRelayPhase::AwaitingAcknowledgement {
                    local_sequence: expected,
                } if *expected == local_sequence => {
                    *phase = InputRelayPhase::Reading;
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
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &mut *phase {
            InputRelayPhase::Reading | InputRelayPhase::Closed => {
                *phase = InputRelayPhase::OutputClosed;
                self.changed.notify_all();
                Ok(())
            }
            InputRelayPhase::OutputClosed => Ok(()),
            InputRelayPhase::Failed(error) => Err(error
                .take()
                .unwrap_or_else(|| anyhow::anyhow!("external connector input relay failed"))),
            InputRelayPhase::Writing { .. }
            | InputRelayPhase::AwaitingAcknowledgement { .. }
            | InputRelayPhase::Closing => {
                bail!("external connector output closed with uncertain input delivery")
            }
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
    let (acknowledgements, acknowledgement_rx) = mpsc::channel::<u64>();
    let input_status = Arc::new(InputRelayStatus::new());
    let relay_status = Arc::clone(&input_status);
    let input_thread = std::thread::Builder::new()
        .name("external-connector-input".into())
        .spawn(move || {
            if let Err(error) = relay_input(writer, acknowledgement_rx, &relay_status) {
                relay_status.fail(error);
                let _ = interrupt.shutdown();
            }
        })?;

    let mut stdout = std::io::stdout().lock();
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
                    acknowledgements
                        .send(local_sequence)
                        .map_err(|_| anyhow::anyhow!("external connector input owner stopped"))?;
                }
                frame @ ExternalConnectorServerFrame::ProtocolBytes { .. } => {
                    let bytes = frame
                        .protocol_bytes()?
                        .context("external connector output frame lost its bytes")?;
                    stdout.write_all(&bytes)?;
                    stdout.flush()?;
                }
                ExternalConnectorServerFrame::ProtocolEof { .. } => {
                    input_status.require_clean_output_eof()?;
                    stdout.flush()?;
                    return Ok(());
                }
                ExternalConnectorServerFrame::Fault { .. } => {
                    bail!("external connector controller reported a closed fault");
                }
            }
        }
    })();
    drop(acknowledgements);
    let _ = stream.shutdown();
    // A completed relay must always be reaped. A relay still blocked on stdin
    // is terminated by process exit after the authenticated output reaches
    // EOF; it owns no authority beyond this process or socket.
    if input_thread.is_finished() {
        input_thread
            .join()
            .map_err(|_| anyhow::anyhow!("external connector input relay panicked"))?;
    }
    result
}

fn relay_input(
    mut stream: lillux::LocalDuplexStream,
    acknowledgements: mpsc::Receiver<u64>,
    status: &InputRelayStatus,
) -> Result<()> {
    let mut stdin = std::io::stdin().lock();
    let mut buffer = vec![0_u8; MAX_CHUNK_BYTES];
    let mut local_sequence = 0_u64;
    loop {
        let read = stdin.read(&mut buffer)?;
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
        write_external_connector_client_frame(
            &mut stream.with_deadline(lillux::time::MonotonicDeadline::after(LOCAL_IO_TIMEOUT)),
            &frame,
        )?;
        if read == 0 {
            status.set(InputRelayPhase::Closed);
            return Ok(());
        }
        status.set(InputRelayPhase::AwaitingAcknowledgement { local_sequence });
        ensure!(
            acknowledgements.recv()? == local_sequence,
            "external connector input acknowledgement changed sequence"
        );
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
        assert!(!status.begin_write(1, false).unwrap());
        assert!(!status.begin_write(2, true).unwrap());
    }
}
