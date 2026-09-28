//! Process-local interactive I/O for one daemon-owned scoped attempt.
//!
//! The caller owns durable ordering and the signed limits. This object owns
//! only the live Lillux channel and bounded capture reader. It cannot recreate
//! either after daemon restart, and no successful write attests guest use.

use std::io::Write as _;
use std::sync::Mutex;

use anyhow::{Context, Result, bail, ensure};

use crate::runtime_db::LaunchOwner;
use crate::runtime_db::scoped_child_attempt::{
    ScopedChildAttemptRecord, ScopedChildInputKind, ScopedChildInputOperation,
    ScopedChildInputReservation, ScopedChildPhase,
};
use crate::state::AppState;

struct InputState {
    channel: lillux::exec::InheritedDuplexChannel,
    closed: bool,
    poisoned: bool,
}

pub struct ScopedProducerInteractiveIo {
    input: Mutex<InputState>,
    /// A separate alias can interrupt a deadline-bound writer without first
    /// acquiring its mutex. Stop still owns the process and scope settlement.
    interrupt: lillux::exec::InheritedDuplexChannel,
    stdout: lillux::exec::ProcessStdoutReader,
}

impl ScopedProducerInteractiveIo {
    pub fn new(
        channel: lillux::exec::InheritedDuplexChannel,
        stdout: lillux::exec::ProcessStdoutReader,
    ) -> Result<Self> {
        let interrupt = channel.try_clone().context("retain scoped input interrupt")?;
        Ok(Self {
            input: Mutex::new(InputState {
                channel,
                closed: false,
                poisoned: false,
            }),
            interrupt,
            stdout,
        })
    }

    /// Must be called only after the exact durable operation was reserved.
    /// On any short/failed write the caller leaves that reservation uncertain;
    /// this channel is poisoned and will not send another byte.
    pub fn write_all_until(
        &self,
        bytes: &[u8],
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        ensure!(!bytes.is_empty(), "scoped input write is empty");
        let mut input = self
            .input
            .lock()
            .map_err(|_| anyhow::anyhow!("scoped input lock poisoned"))?;
        ensure!(!input.closed && !input.poisoned, "scoped input is closed or uncertain");
        if let Err(error) = input.channel.with_deadline(deadline).write_all(bytes) {
            input.poisoned = true;
            let _ = input.channel.shutdown();
            return Err(error).context("scoped input delivery is uncertain");
        }
        Ok(())
    }

    /// Must be called only after an ordered durable CLOSE reservation. This
    /// half-close leaves stdout readable while the target drains and exits.
    pub fn close_input(&self) -> Result<()> {
        let mut input = self
            .input
            .lock()
            .map_err(|_| anyhow::anyhow!("scoped input lock poisoned"))?;
        ensure!(!input.closed && !input.poisoned, "scoped input is closed or uncertain");
        if let Err(error) = input.channel.shutdown_write() {
            input.poisoned = true;
            let _ = input.channel.shutdown();
            return Err(error).context("scoped input close is uncertain");
        }
        input.closed = true;
        Ok(())
    }

    /// Read by explicit byte coordinate so a lost response can repeat the
    /// same capture slice without advancing a hidden cursor or contacting the
    /// target. The daemon service must cap this length by the signed recipe.
    pub fn read_from_until(
        &self,
        offset: usize,
        maximum_bytes: usize,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Vec<u8>> {
        if maximum_bytes == 0 || maximum_bytes > 64 * 1024 {
            bail!("scoped stdout read length is not bounded");
        }
        let mut bytes = vec![0; maximum_bytes];
        let count = self
            .stdout
            .read_from_until(offset, &mut bytes, deadline)
            .context("read scoped stdout capture")?;
        bytes.truncate(count);
        Ok(bytes)
    }

    /// Stop's nonblocking wake path; it never claims that the target exited.
    pub fn interrupt(&self) -> Result<()> {
        self.interrupt.shutdown().context("interrupt scoped input")
    }
}

fn exact_live_io(
    state: &AppState,
    attempt_id: &str,
    owner: &LaunchOwner,
) -> Result<(
    ScopedChildAttemptRecord,
    std::sync::Arc<ScopedProducerInteractiveIo>,
    ryeos_state::external_content::products::producer_recipe::ProductProducerRecipe,
    lillux::time::MonotonicDeadline,
)> {
    let record = state
        .state_store
        .scoped_child_attempt(attempt_id)?
        .context("scoped interactive attempt is absent")?;
    ensure!(
        record.initial.owner == *owner && record.phase == ScopedChildPhase::ReleasePermitted,
        "scoped interactive attempt is not the exact released owner"
    );
    let (io, recipe, deadline) = state
        .scoped_producer_processes
        .interactive_io_exact(&record)?
        .context("scoped interactive process has no exact live channel")?;
    ensure!(
        recipe.digest()? == record.initial.recipe_digest,
        "scoped interactive recipe differs from retained attempt"
    );
    Ok((record, io, recipe, deadline))
}

/// Reserve before contact. A delivered replay returns its previous local
/// delivery acknowledgment; a pending/uncertain replay never resends bytes.
pub fn write_scoped_producer(
    state: &AppState,
    attempt_id: &str,
    owner: &LaunchOwner,
    sequence: u32,
    bytes: &[u8],
    deadline: lillux::time::MonotonicDeadline,
) -> Result<ScopedChildInputOperation> {
    let byte_count = u32::try_from(bytes.len()).context("scoped input frame is too large")?;
    let operation = ScopedChildInputOperation {
        sequence,
        kind: ScopedChildInputKind::Write,
        payload_digest: lillux::sha256_hex(bytes),
        byte_count,
    };
    if state
        .state_store
        .delivered_scoped_child_input_operation(attempt_id, owner, &operation)?
    {
        return Ok(operation);
    }
    let (_, io, recipe, child_deadline) = exact_live_io(state, attempt_id, owner)?;
    ensure!(
        !deadline.has_elapsed() && !child_deadline.has_elapsed(),
        "scoped input deadline elapsed before reservation"
    );
    let reservation = state.state_store.reserve_scoped_child_input_operation(
        attempt_id,
        owner,
        &operation,
        &recipe.stdin_source,
    )?;
    if reservation == ScopedChildInputReservation::AlreadyDelivered {
        return Ok(operation);
    }
    // Stop/observe may have won after reservation. Do not contact a retained
    // Arc unless it still belongs to the exact live registry slot.
    let (_, current, _, _) = exact_live_io(state, attempt_id, owner)?;
    ensure!(
        std::sync::Arc::ptr_eq(&io, &current),
        "scoped interactive channel changed after input reservation"
    );
    io.write_all_until(bytes, deadline.min(child_deadline))?;
    state
        .state_store
        .complete_scoped_child_input_operation(attempt_id, owner, &operation)?;
    Ok(operation)
}

/// Ordered input EOF, separate from process exit or scope settlement.
pub fn close_scoped_producer_input(
    state: &AppState,
    attempt_id: &str,
    owner: &LaunchOwner,
    sequence: u32,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<ScopedChildInputOperation> {
    let operation = ScopedChildInputOperation {
        sequence,
        kind: ScopedChildInputKind::Close,
        payload_digest: lillux::sha256_hex(b""),
        byte_count: 0,
    };
    if state
        .state_store
        .delivered_scoped_child_input_operation(attempt_id, owner, &operation)?
    {
        return Ok(operation);
    }
    let (_, io, recipe, child_deadline) = exact_live_io(state, attempt_id, owner)?;
    ensure!(
        !deadline.has_elapsed() && !child_deadline.has_elapsed(),
        "scoped input deadline elapsed before close reservation"
    );
    let reservation = state.state_store.reserve_scoped_child_input_operation(
        attempt_id,
        owner,
        &operation,
        &recipe.stdin_source,
    )?;
    if reservation == ScopedChildInputReservation::AlreadyDelivered {
        return Ok(operation);
    }
    let (_, current, _, _) = exact_live_io(state, attempt_id, owner)?;
    ensure!(
        std::sync::Arc::ptr_eq(&io, &current),
        "scoped interactive channel changed after close reservation"
    );
    ensure!(
        !deadline.has_elapsed() && !child_deadline.has_elapsed(),
        "scoped input deadline elapsed before close"
    );
    io.close_input()?;
    state
        .state_store
        .complete_scoped_child_input_operation(attempt_id, owner, &operation)?;
    Ok(operation)
}

/// Read only the signed bounded capture of the exact live process. A repeat
/// at the same offset is safe after a lost response; no child I/O is replayed.
pub fn read_scoped_producer_stdout(
    state: &AppState,
    attempt_id: &str,
    owner: &LaunchOwner,
    offset: usize,
    maximum_bytes: usize,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<Vec<u8>> {
    let (_, io, recipe, child_deadline) = exact_live_io(state, attempt_id, owner)?;
    ensure!(
        !deadline.has_elapsed() && !child_deadline.has_elapsed(),
        "scoped stdout deadline elapsed"
    );
    let bound = usize::try_from(recipe.bounds.maximum_stdout_bytes)
        .context("scoped stdout signed bound is not representable")?;
    ensure!(
        maximum_bytes > 0
            && maximum_bytes <= 64 * 1024
            && maximum_bytes <= bound
            && offset <= bound,
        "scoped stdout read exceeds signed capture bound"
    );
    let bytes = io.read_from_until(offset, maximum_bytes, deadline.min(child_deadline))?;
    let (_, current, _, _) = exact_live_io(state, attempt_id, owner)?;
    ensure!(
        std::sync::Arc::ptr_eq(&io, &current),
        "scoped interactive process moved during stdout read"
    );
    Ok(bytes)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn direct_channel_repeats_capture_and_half_closes_input() {
        let (parent, child) = lillux::inherited_duplex_channel_pair().unwrap();
        let mut request = lillux::SubprocessRequest {
            cmd: "/bin/cat".into(),
            argv0: None,
            args: Vec::new(),
            cwd: None,
            envs: Vec::new(),
            stdin_data: None,
            timeout: 5.0,
            limits: Some(lillux::SubprocessLimits {
                max_stdout_bytes: Some(1024),
                ..Default::default()
            }),
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        };
        child.bind_as_subprocess_stdin(&mut request).unwrap();
        drop(child);
        let mut process = lillux::spawn(request).unwrap();
        let stdout = process.take_stdout_reader().unwrap();
        let io = ScopedProducerInteractiveIo::new(parent, stdout).unwrap();
        let deadline = || lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(2));
        io.write_all_until(b"scoped\n", deadline()).unwrap();
        io.close_input().unwrap();
        assert!(io.write_all_until(b"later", deadline()).is_err());
        assert_eq!(io.read_from_until(0, 7, deadline()).unwrap(), b"scoped\n");
        assert_eq!(io.read_from_until(0, 7, deadline()).unwrap(), b"scoped\n");
        assert!(process.wait().success);
        assert_eq!(io.read_from_until(7, 7, deadline()).unwrap(), b"");
    }
}
