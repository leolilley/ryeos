//! Retained same-root host for an admitted, interpreted semantic checker.
//!
//! Native code owns source/byte custody, finite framing and direct-child joins.
//! The selected script alone interprets product meaning. No local success
//! attests enclosing root/process-scope settlement or issues qualification.
use anyhow::{Context as _, Result, bail, ensure};
use lillux::exec::{
    CooperativeChildTermination, InheritedDescriptorAuthority, PendingCooperativeChildTermination,
    SubprocessLimits,
};
use lillux::subordinate_process::{
    PinnedSubordinateProcessRequest, RootTerminalSubordinateFailure, SubordinateDiagnosticDrain,
    SubordinateDiagnosticDrainEnd, SubordinateProcess, SubordinateProcessInput,
    SubordinateProcessOutput,
};
use lillux::task::HostTask;
use lillux::time::MonotonicDeadline;
use ryeos_state::objects::ExternalContentRealizationSet;
use serde::Serialize;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crate::consumer_checker_program::{CheckerProgramCustody, prepare_checker_program};
use crate::consumer_checker_projection::{PreparedCheckerInput, decode_checker_response};
use crate::consumer_record::ConsumerInputRecord;
use crate::consumer_worker_semantics::CheckedQConversation;
use crate::consumer_worker_transcript::{
    ProtectedConsumerWorkerChunks, VerifiedConsumerWorkerTranscript,
};
use crate::finite_product_contract::FiniteProductContract;

const MAX_HEADER_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const HEADER_MAGIC: &[u8; 9] = b"RYCKH3V1\n";
const TABLE_SCHEMA: &str = "ryeos.checker_fd_table.v1";

/// Opaque complete local host join. It retains P's sole parsed transcript and
/// original bytes; the public interface never exposes P's private owner type.
pub struct CheckerHostSuccess {
    checked: VerifiedQConversation,
}

impl CheckerHostSuccess {
    pub fn transcript(&self) -> &VerifiedConsumerWorkerTranscript {
        self.checked.prepared.transcript()
    }
    pub fn checked_q(&self) -> &VerifiedQConversation {
        &self.checked
    }
    pub(crate) fn original_record(&self) -> &ConsumerInputRecord {
        self.checked.prepared.original_record()
    }
}

/// Product meaning joined to exact protected input AND a completed local child.
/// No public constructor, deserializer or clone can manufacture this owner.
#[derive(Serialize)]
pub struct VerifiedQConversation {
    #[serde(skip)]
    prepared: Box<PreparedCheckerInput>,
    #[serde(flatten)]
    conversation: CheckedQConversation,
    #[serde(skip)]
    original_record_sha256: String,
    #[serde(skip)]
    transcript_manifest_hash: String,
    #[serde(skip)]
    transcript_stream_digest: String,
}

impl std::fmt::Debug for VerifiedQConversation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VerifiedQConversation(retained local host join)")
    }
}

impl VerifiedQConversation {
    // Private to this module. Only run_transaction calls this AFTER EOF,
    // zero exit, exact reap, diagnostic EOF and the five P response joins.
    fn from_complete_host_join(
        prepared: Box<PreparedCheckerInput>,
        conversation: CheckedQConversation,
    ) -> Result<Self> {
        // P's ONE verify_chunks already joined the manifest digest to the
        // retained canonical bytes. The same Box keeps transcript allocation
        // identity stable while wrappers move; no whole-stream reconstruction.
        let original_record_sha256 =
            lillux::sha256_hex(&prepared.original_record().canonical_bytes()?);
        let transcript_manifest_hash = prepared.transcript().reference.manifest_hash.clone();
        let transcript_stream_digest = prepared
            .transcript()
            .reference
            .manifest
            .stream_sha256
            .clone();
        Ok(Self {
            prepared,
            conversation,
            original_record_sha256,
            transcript_manifest_hash,
            transcript_stream_digest,
        })
    }

    pub(crate) fn require_for(
        &self,
        original: &ConsumerInputRecord,
        transcript: &VerifiedConsumerWorkerTranscript,
    ) -> Result<()> {
        // Native lifetime/allocation join only, never a raw-pointer authority
        // or cross-process claim. The private Box owns this immutable typed
        // transcript; a cloned/forged public DTO with matching hashes refuses.
        ensure!(
            std::ptr::eq(transcript, self.prepared.transcript())
                && self.original_record_sha256 == lillux::sha256_hex(&original.canonical_bytes()?)
                && self.transcript_manifest_hash == transcript.reference.manifest_hash
                && self.transcript_stream_digest == transcript.reference.manifest.stream_sha256,
            "checked Q conversation differs from retained host input"
        );
        Ok(())
    }
}

#[derive(Default)]
struct CancellationTokens {
    request: Option<CooperativeChildTermination>,
    pending: Option<PendingCooperativeChildTermination>,
}

#[derive(Default)]
struct Cancellation {
    requested: AtomicBool,
    poisoned: AtomicBool,
    tokens: Mutex<CancellationTokens>,
}

impl Cancellation {
    fn take_request(&self) -> Option<CooperativeChildTermination> {
        // This lock owns only a one-shot token. No IO, wait or process request
        // is performed while it is held; it is separate from attempt custody.
        match self.tokens.lock() {
            Ok(mut tokens) => tokens.request.take(),
            Err(error) => {
                self.poisoned.store(true, Ordering::Release);
                error.into_inner().request.take()
            }
        }
    }

    fn request(&self) {
        self.requested.store(true, Ordering::Release);
        if let Some(token) = self.take_request() {
            // SIGTERM is a request only. It does not reap or prove pipe EOF,
            // child/descendant death or any applied scope settlement.
            match token.request() {
                Ok(pending) => match self.tokens.lock() {
                    Ok(mut tokens) => tokens.pending = Some(pending),
                    Err(error) => {
                        self.poisoned.store(true, Ordering::Release);
                        error.into_inner().pending = Some(pending);
                    }
                },
                Err(_) => {
                    self.poisoned.store(true, Ordering::Release);
                }
            }
        }
    }

    fn publish(&self, token: CooperativeChildTermination) -> Result<()> {
        {
            let mut tokens = self
                .tokens
                .lock()
                .map_err(|_| anyhow::anyhow!("checker cancellation slot poisoned"))?;
            ensure!(
                tokens.request.is_none() && tokens.pending.is_none(),
                "checker token was already published"
            );
            tokens.request = Some(token);
        }
        // Cancellation racing successful spawn/publication cannot be lost.
        if self.requested.load(Ordering::Acquire) {
            self.request();
        }
        ensure!(
            !self.poisoned.load(Ordering::Acquire),
            "checker cancellation request failed"
        );
        Ok(())
    }

    fn require_running(&self, deadline: MonotonicDeadline) -> Result<()> {
        ensure!(
            !deadline.has_elapsed(),
            "checker original caller deadline expired"
        );
        ensure!(
            !self.requested.load(Ordering::Acquire) && !self.poisoned.load(Ordering::Acquire),
            "checker invocation cancelled or poisoned"
        );
        Ok(())
    }
}

#[derive(Default)]
struct Attempt {
    child: Option<SubordinateProcess>,
    terminal_child: Option<RootTerminalSubordinateFailure>,
    input: Option<SubordinateProcessInput>,
    output: Option<SubordinateProcessOutput>,
    drain: Option<SubordinateDiagnosticDrain>,
    program: Option<CheckerProgramCustody>,
    prepared: Option<Box<PreparedCheckerInput>>,
    descriptors: Vec<InheritedDescriptorAuthority>,
}

impl Attempt {
    fn settle_refusal(&mut self, deadline: MonotonicDeadline) -> bool {
        // Refusal owns exact-child forceful cleanup. No process-group signal
        // or renewed cleanup budget is introduced.
        let reaped = match self.child.as_mut() {
            Some(child) => matches!(child.kill_exact_child_until(deadline), Ok(Some(_))),
            None => self.terminal_child.is_none(),
        };
        let drained = match self.drain.as_mut() {
            Some(drain) => drain.cancel_until(deadline).is_some(),
            None => true, // no diagnostic task was successfully started
        };
        if reaped && drained {
            self.child.take(); // already reaped: ordinary Drop never waits
            self.input.take();
            self.output.take();
            self.drain.take(); // explicit completion already joined its task
            true
        } else {
            if let Some(child) = self.child.take() {
                self.terminal_child = Some(child.into_root_terminal_failure());
            }
            // Keep taken IO, diagnostic task, program and sealed descriptors.
            false
        }
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        // Last-resort owner destruction must never invoke the ordinary child's
        // unbounded kill/wait Drop, even after unexpected Rust unwind. This is
        // loss of observation at root exit, never successful cleanup evidence.
        if let Some(child) = self.child.take() {
            self.terminal_child = Some(child.into_root_terminal_failure());
        }
    }
}

struct SharedAttempt {
    // Only the host worker holds this guard across bounded IO. Parent/root
    // NEVER acquires it for cancellation, refusal or terminal conversion.
    state: Mutex<Attempt>,
    cancellation: Arc<Cancellation>,
    locally_settled: AtomicBool,
}

impl SharedAttempt {
    fn new() -> Self {
        Self {
            state: Mutex::new(Attempt::default()),
            cancellation: Arc::new(Cancellation::default()),
            locally_settled: AtomicBool::new(true),
        }
    }
}

/// Instantiate in synchronous main BEFORE constructing Tokio. It retains the
/// attempt and live HostTask outside every await, future, worker and IO frame.
pub struct CheckerInvocationOwner {
    shared: Arc<SharedAttempt>,
    task: Option<HostTask<Result<CheckerHostSuccess>>>,
    started: bool,
}

impl Default for CheckerInvocationOwner {
    fn default() -> Self {
        Self {
            shared: Arc::new(SharedAttempt::new()),
            task: None,
            started: false,
        }
    }
}

/// Typed unresolved root-terminal custody. Debug performs no lock/IO and emits
/// no source, provider, script response or panic payload. Retain until root exit.
#[must_use = "retain unresolved checker/task/IO ownership through root terminal exit"]
pub struct CheckerTerminalFailure {
    _shared: Arc<SharedAttempt>,
    _task: Option<HostTask<Result<CheckerHostSuccess>>>,
}

impl std::fmt::Debug for CheckerTerminalFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CheckerTerminalFailure(unresolved retained root custody)")
    }
}

struct AwaitCancellationGuard {
    cancellation: Arc<Cancellation>,
    armed: bool,
}

impl Drop for AwaitCancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.request();
        }
    }
}

impl CheckerInvocationOwner {
    pub fn request_cancellation(&self) {
        self.shared.cancellation.request();
    }

    /// Consume the root owner after the runtime/future has returned or unwound.
    /// A live task is retained, never joined/detached as a normal refusal path.
    /// Only an already finished task may be joined here, with no state lock.
    pub fn into_terminal_failure(mut self) -> Option<CheckerTerminalFailure> {
        if self.task.as_ref().is_some_and(HostTask::is_finished) {
            let task = self.task.take().expect("observed finished checker task");
            if task.join().is_err() {
                self.shared.locally_settled.store(false, Ordering::Release);
            }
        }
        if self.task.is_some() || !self.shared.locally_settled.load(Ordering::Acquire) {
            self.request_cancellation();
            Some(CheckerTerminalFailure {
                _shared: Arc::clone(&self.shared),
                _task: self.task.take(),
            })
        } else {
            None
        }
    }

    pub(crate) async fn invoke(
        &mut self,
        original: ConsumerInputRecord,
        acquired: ProtectedConsumerWorkerChunks,
    ) -> Result<CheckerHostSuccess> {
        let deadline = acquired.original_deadline();
        ensure!(
            !self.started && self.task.is_none(),
            "checker root invocation cannot be reused"
        );
        self.shared.cancellation.require_running(deadline)?;
        self.started = true;
        self.shared.locally_settled.store(false, Ordering::Release);
        let shared = Arc::clone(&self.shared);
        let task = lillux::task::spawn_host_task("codex-semantic-checker", move || {
            run_worker(&shared, original, acquired, deadline)
        });
        match task {
            Ok(task) => self.task = Some(task),
            Err(error) => {
                self.shared.locally_settled.store(true, Ordering::Release);
                return Err(error.into());
            }
        }
        self.await_task(deadline).await
    }

    // Owns only the await guard. Task/attempt custody stays in synchronous
    // main's owner even when this future is dropped or times out.
    async fn await_task(&mut self, deadline: MonotonicDeadline) -> Result<CheckerHostSuccess> {
        let mut guard = AwaitCancellationGuard {
            cancellation: Arc::clone(&self.shared.cancellation),
            armed: true,
        };
        loop {
            if self
                .task
                .as_ref()
                .expect("root retains checker task")
                .is_finished()
            {
                // Never blocking-join an unfinished task on current-thread Tokio.
                let task = self.task.take().expect("observed finished checker task");
                let result = task
                    .join()
                    .map_err(|_| anyhow::anyhow!("checker host task panicked"));
                if result.is_err() {
                    self.shared.locally_settled.store(false, Ordering::Release);
                }
                self.shared.cancellation.require_running(deadline)?;
                guard.armed = false;
                return result?;
            }
            self.shared.cancellation.require_running(deadline)?;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }
}

impl Drop for CheckerInvocationOwner {
    fn drop(&mut self) {
        // Main explicitly consumes this owner into typed terminal custody. A
        // surprising outer unwind only requests cancellation; it never waits.
        if self.task.is_some() {
            self.request_cancellation();
        }
    }
}

fn run_worker(
    shared: &SharedAttempt,
    original: ConsumerInputRecord,
    acquired: ProtectedConsumerWorkerChunks,
    deadline: MonotonicDeadline,
) -> Result<CheckerHostSuccess> {
    run_owned_transaction(shared, deadline, |state, cancellation| {
        run_transaction(state, cancellation, original, acquired, deadline)
    })
}

fn run_owned_transaction(
    shared: &SharedAttempt,
    deadline: MonotonicDeadline,
    transaction: impl FnOnce(&mut Attempt, &Cancellation) -> Result<CheckerHostSuccess>,
) -> Result<CheckerHostSuccess> {
    let mut state = match shared.state.lock() {
        Ok(state) => state,
        Err(error) => {
            let mut state = error.into_inner();
            let settled = state.settle_refusal(deadline);
            shared.locally_settled.store(settled, Ordering::Release);
            bail!("checker attempt state poisoned");
        }
    };
    // Catch inside the retained guard: unwinding cannot drop the sole child,
    // taken streams or diagnostic owner. Parent never takes this IO guard.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        transaction(&mut state, &shared.cancellation)
    }));
    match outcome {
        Ok(Ok(success)) => {
            shared.locally_settled.store(true, Ordering::Release);
            Ok(success)
        }
        Ok(Err(error)) => {
            let settled = state.settle_refusal(deadline);
            shared.locally_settled.store(settled, Ordering::Release);
            Err(error)
        }
        Err(_) => {
            let settled = state.settle_refusal(deadline);
            shared.locally_settled.store(settled, Ordering::Release);
            bail!("checker transaction panicked; no semantic result")
        }
    }
}

/// This handoff is authenticated by the admitted root's protected Executor
/// PerSpawnDaemon launch binding, not by DTO parsing. No callback/argv/path
/// replacement or source/code-map substitution is accepted.
fn admitted_root_realizations() -> Result<ExternalContentRealizationSet> {
    let raw = std::env::var("RYEOS_EXTERNAL_REALIZATIONS")
        .context("admitted root checker dependencies absent")?;
    let value = ryeos_external_execution_contract::from_json_slice_strict(
        raw.as_bytes(),
        MAX_HEADER_BYTES,
    )?;
    ensure!(
        lillux::canonical_json(&value)? == raw,
        "admitted root realization set is not canonical"
    );
    ExternalContentRealizationSet::from_value(&value)
}

fn run_transaction(
    state: &mut Attempt,
    cancellation: &Cancellation,
    original: ConsumerInputRecord,
    acquired: ProtectedConsumerWorkerChunks,
    deadline: MonotonicDeadline,
) -> Result<CheckerHostSuccess> {
    cancellation.require_running(deadline)?;
    let contract: FiniteProductContract = original.product_contract()?;
    let root_set = admitted_root_realizations()?;
    let program = prepare_checker_program(&contract, &root_set)?;
    cancellation.require_running(deadline)?;
    let prepared =
        acquired.prepare_checker_input(original, program.checker_identity_digest().to_owned())?;
    state.prepared = Some(Box::new(prepared));
    let (loader, custody) = program.into_launch_parts();
    state.program = Some(custody);
    cancellation.require_running(deadline)?;
    let (request, header_fd) = prepare_launch(state, loader)?;
    cancellation.require_running(deadline)?;
    // No fallible operation between a successful spawn and retained ownership.
    state.child = Some(SubordinateProcess::spawn_pinned(request).map_err(anyhow::Error::msg)?);
    let token = state
        .child
        .as_ref()
        .expect("retained successful spawn")
        .cooperative_termination()
        .map_err(anyhow::Error::msg)?;
    cancellation.publish(token)?;
    // Start diagnostics before input/response processing; zero retained bytes.
    let error = state
        .child
        .as_mut()
        .expect("retained child")
        .take_error()
        .map_err(anyhow::Error::msg)?;
    state.drain = Some(error.start_discarding()?);
    state.output = Some(
        state
            .child
            .as_mut()
            .expect("retained child")
            .take_output()
            .map_err(anyhow::Error::msg)?,
    );
    state.input = Some(
        state
            .child
            .as_mut()
            .expect("retained child")
            .take_input()
            .map_err(anyhow::Error::msg)?,
    );
    cancellation.require_running(deadline)?;
    let startup = format!("{header_fd}\n");
    state
        .input
        .as_mut()
        .expect("retained startup writer")
        .write_all_until(startup.as_bytes(), deadline)?;
    // Close only this completed input pipe. Bootstrap requires exact stdin EOF.
    state.input.take();
    cancellation.require_running(deadline)?;
    let response = read_and_settle_checker(state, cancellation, deadline)?;
    let checked =
        decode_checker_response(&response, state.prepared.as_ref().expect("retained input"))?;
    cancellation.require_running(deadline)?;
    // Only now consume the SAME boxed prepared input into the private verified
    // owner. No independent parsed transcript or raw-pointer token escapes.
    let verified = VerifiedQConversation::from_complete_host_join(
        state.prepared.take().expect("sole retained boxed input"),
        checked,
    )?;
    cancellation.require_running(deadline)?;
    // These owners are explicitly settled before the result leaves the worker.
    state.child.take();
    state.output.take();
    state.drain.take();
    Ok(CheckerHostSuccess { checked: verified })
}

// This primitive joins actual Lillux IO/child observations, never semantic
// meaning or runtime conformance. It cannot construct a verified result.
fn read_and_settle_checker(
    state: &mut Attempt,
    cancellation: &Cancellation,
    deadline: MonotonicDeadline,
) -> Result<Vec<u8>> {
    let response = state
        .output
        .as_mut()
        .expect("retained response reader")
        .read_frame_until(b'\n', MAX_RESPONSE_BYTES, deadline)?;
    cancellation.require_running(deadline)?;
    let trailing = state
        .output
        .as_mut()
        .expect("retained response reader")
        .read_frame_until(b'\n', 1, deadline)?;
    ensure!(
        trailing.is_empty(),
        "checker emitted bytes after its response"
    );
    cancellation.require_running(deadline)?;
    let exit = state
        .child
        .as_mut()
        .expect("retained child")
        .wait_exact_child_until(deadline)
        .map_err(anyhow::Error::msg)?
        .context("checker exact-child reap incomplete")?;
    ensure!(
        exit.success && exit.code == Some(0),
        "checker refused or exited unsuccessfully"
    );
    ensure!(
        state
            .drain
            .as_mut()
            .expect("retained diagnostic drain")
            .finish_until(deadline)
            == Some(SubordinateDiagnosticDrainEnd::Eof),
        "checker diagnostic EOF/join incomplete"
    );
    cancellation.require_running(deadline)?;
    Ok(response)
}

#[derive(Serialize)]
struct ChunkDescriptor {
    index: usize,
    fd: u32,
    bytes: usize,
}
#[derive(Serialize)]
struct ScriptDescriptor<'a> {
    fd: u32,
    bytes: u64,
    sha256: &'a str,
}
#[derive(Serialize)]
struct DescriptorTable<'a> {
    schema: &'static str,
    script: ScriptDescriptor<'a>,
    chunks: Vec<ChunkDescriptor>,
}

fn decimal_bytes(mut number: u64) -> usize {
    let mut size = 1;
    while number >= 10 {
        number /= 10;
        size += 1;
    }
    size
}
fn add_size(total: &mut usize, extra: usize) -> Result<()> {
    *total = total
        .checked_add(extra)
        .context("checker header byte count overflow")?;
    Ok(())
}
fn table_encoded_size(table: &DescriptorTable<'_>) -> Result<usize> {
    // Exact canonical key order and ASCII tokens; only unsigned decimal digits
    // and an already validated 64-hex digest are variable. No content clone.
    ensure!(
        table.schema == TABLE_SCHEMA
            && table.script.sha256.len() == 64
            && table
                .script
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "checker table metadata is not canonical"
    );
    let mut size = b"{\"chunks\":[".len();
    for (index, chunk) in table.chunks.iter().enumerate() {
        ensure!(
            chunk.index == index && chunk.fd > 2,
            "checker chunk descriptor coordinate changed"
        );
        if index != 0 {
            add_size(&mut size, 1)?;
        }
        add_size(&mut size, b"{\"bytes\":".len())?;
        add_size(&mut size, decimal_bytes(u64::try_from(chunk.bytes)?))?;
        add_size(&mut size, b",\"fd\":".len())?;
        add_size(&mut size, decimal_bytes(u64::from(chunk.fd)))?;
        add_size(&mut size, b",\"index\":".len())?;
        add_size(&mut size, decimal_bytes(u64::try_from(chunk.index)?))?;
        add_size(&mut size, 1)?;
    }
    add_size(
        &mut size,
        b"],\"schema\":\"ryeos.checker_fd_table.v1\",\"script\":{\"bytes\":".len(),
    )?;
    add_size(&mut size, decimal_bytes(table.script.bytes))?;
    add_size(&mut size, b",\"fd\":".len())?;
    add_size(&mut size, decimal_bytes(u64::from(table.script.fd)))?;
    add_size(&mut size, b",\"sha256\":\"".len())?;
    add_size(&mut size, table.script.sha256.len())?;
    add_size(&mut size, b"\"}}".len())?;
    Ok(size)
}
fn combined_header_size(metadata: usize, table: usize, maximum: usize) -> Result<usize> {
    ensure!(
        maximum == MAX_HEADER_BYTES,
        "checker header cap differs from fixed native contract"
    );
    let mut total = HEADER_MAGIC.len();
    for extra in [4, metadata, 4, table] {
        add_size(&mut total, extra)?;
    }
    ensure!(
        total <= maximum,
        "combined checker metadata/table/framing exceeds header cap"
    );
    Ok(total)
}

fn prepare_launch(
    state: &mut Attempt,
    loader: lillux::PinnedRegularFile,
) -> Result<(PinnedSubordinateProcessRequest, u32)> {
    let program = state
        .program
        .as_ref()
        .context("retained checker program absent")?;
    let prepared = state
        .prepared
        .as_ref()
        .context("retained checker input absent")?;
    let selector = &program.selector;
    selector.validate()?;
    ensure!(
        selector.max_response_bytes == MAX_RESPONSE_BYTES as u64,
        "checker response cap differs from native contract"
    );
    let interpreter = program.interpreter.inherited_descriptor_authority()?;
    let script = lillux::exec::sealed_memfd(c"ryeos-checker-script", &program.script_bytes)
        .map_err(anyhow::Error::msg)?;
    let mut table = DescriptorTable {
        schema: TABLE_SCHEMA,
        script: ScriptDescriptor {
            fd: script.inherited_descriptor().map_err(anyhow::Error::msg)?,
            bytes: selector.code.script.bytes,
            sha256: &selector.code.script.sha256,
        },
        chunks: Vec::with_capacity(prepared.original_chunks().len()),
    };
    let mut descriptors = vec![interpreter.clone(), script];
    for (index, bytes) in prepared.original_chunks().iter().enumerate() {
        let fd = lillux::exec::sealed_memfd(c"ryeos-checker-chunk", bytes)
            .map_err(anyhow::Error::msg)?;
        table.chunks.push(ChunkDescriptor {
            index,
            fd: fd.inherited_descriptor().map_err(anyhow::Error::msg)?,
            bytes: bytes.len(),
        });
        descriptors.push(fd);
    }
    // Preflight ALL framing and table bytes before table serialization or any
    // combined header allocation/seal, even when P consumes its full 2MiB cap.
    let table_size = table_encoded_size(&table)?;
    let total = combined_header_size(
        prepared.metadata_header().len(),
        table_size,
        usize::try_from(selector.max_header_bytes)?,
    )?;
    let table_bytes = lillux::canonical_json(&serde_json::to_value(&table)?)?.into_bytes();
    ensure!(
        table_bytes.len() == table_size,
        "checker table encoding differs from preflight"
    );
    let mut header = Vec::with_capacity(total);
    header.extend_from_slice(HEADER_MAGIC);
    header.extend_from_slice(&u32::try_from(prepared.metadata_header().len())?.to_be_bytes());
    header.extend_from_slice(prepared.metadata_header());
    header.extend_from_slice(&u32::try_from(table_bytes.len())?.to_be_bytes());
    header.extend_from_slice(&table_bytes);
    ensure!(
        header.len() == total,
        "checker combined header fill differs from preflight"
    );
    let header =
        lillux::exec::sealed_memfd(c"ryeos-checker-header", &header).map_err(anyhow::Error::msg)?;
    let header_fd = header.inherited_descriptor().map_err(anyhow::Error::msg)?;
    descriptors.push(header);
    let library = program
        .library_directory
        .verified_read_only_namespace_path()?;
    let prefix = program
        .prefix_directory
        .verified_read_only_namespace_path()?;
    let args = vec![
        "--library-path".to_owned(),
        library
            .to_str()
            .context("checker library path is not UTF-8")?
            .to_owned(),
        interpreter
            .path()
            .to_str()
            .context("checker interpreter FD path is not UTF-8")?
            .to_owned(),
        "-I".to_owned(),
        "-S".to_owned(),
        "-B".to_owned(),
        "-X".to_owned(),
        "utf8".to_owned(),
        "-u".to_owned(),
        "-c".to_owned(),
        TRANSPORT_BOOTSTRAP.to_owned(),
        prefix
            .to_str()
            .context("checker prefix is not UTF-8")?
            .to_owned(),
    ];
    let cwd = program.runtime_root.try_clone()?;
    // Retain registered descriptor authorities as well as request copies until
    // local child settlement; inheritance never reopens paths or grants scope.
    state.descriptors = descriptors.clone();
    Ok((
        PinnedSubordinateProcessRequest {
            executable: loader,
            cwd,
            argv0: Some("ryeos-admitted-checker-loader".to_owned()),
            args,
            envs: Vec::new(),
            limits: Some(SubprocessLimits {
                deny_process_creation: true,
                ..Default::default()
            }),
            inherited_fds: descriptors,
        },
        header_fd,
    ))
}

// Mechanical transport only: no Codex method, notification role, expected
// fixture comparison or product acceptance rule belongs in this literal.
// Real exact-runtime FD-prefix/startup namespace conformance is an independent
// pre-signing/admission gate. Post-start checks below do not prove confinement
// before CPython's first import and never substitute for that gate.
const TRANSPORT_BOOTSTRAP: &str = r#"
import sys

def require(value):
    if not value:
        raise ValueError('checker transport refused')

require(len(sys.argv) == 2)
prefix = sys.argv[1]
require(prefix.startswith('/') and not prefix.endswith('/'))
require(sys.flags.isolated == 1 and sys.flags.no_site == 1 and sys.flags.dont_write_bytecode == 1)
require(sys.prefix == prefix and sys.exec_prefix == prefix)
require(sys.base_prefix == prefix and sys.base_exec_prefix == prefix)
require(sys.path and all(type(p) is str and p.startswith(prefix + '/') for p in sys.path))

def require_module_origins():
    for module in tuple(sys.modules.values()):
        spec = getattr(module, '__spec__', None)
        origin = getattr(spec, 'origin', None)
        require(origin is None or origin in ('built-in', 'frozen') or
                (type(origin) is str and origin.startswith(prefix + '/')))

require_module_origins()
import json
import hashlib
require_module_origins()

def exact_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result)
        result[key] = value
    return result

def decode(data):
    return json.loads(data, object_pairs_hook=exact_object,
                      parse_constant=lambda value: (_ for _ in ()).throw(ValueError('nonfinite transport JSON')))

def integer(value, minimum, maximum):
    require(type(value) is int and minimum <= value <= maximum)
    return value

def read_exact_fd(fd, length, maximum):
    integer(fd, 3, 2147483647)
    integer(length, 0, maximum)
    with open(fd, 'rb', closefd=False) as source:
        data = source.read(length + 1)
    require(len(data) == length)
    return data

line = sys.stdin.buffer.readline(32)
require(line.endswith(b'\n') and line[:-1].isdigit() and sys.stdin.buffer.read(1) == b'')
header_fd = integer(int(line[:-1]), 3, 2147483647)
with open(header_fd, 'rb', closefd=False) as source:
    header = source.read(2097153)
require(17 <= len(header) <= 2097152 and header[:9] == b'RYCKH3V1\n')
metadata_size = int.from_bytes(header[9:13], 'big')
end = 13 + metadata_size
require(end + 4 <= len(header))
table_size = int.from_bytes(header[end:end + 4], 'big')
require(end + 4 + table_size == len(header))
envelope = decode(header[13:end])
table = decode(header[end + 4:])
require(type(table) is dict and set(table) == {'schema', 'script', 'chunks'})
require(table['schema'] == 'ryeos.checker_fd_table.v1')
script = table['script']
require(type(script) is dict and set(script) == {'fd', 'bytes', 'sha256'})
require(type(script['sha256']) is str and len(script['sha256']) == 64)
script_bytes = read_exact_fd(script['fd'], script['bytes'], 33554432)
require(hashlib.sha256(script_bytes).hexdigest() == script['sha256'])
chunks = table['chunks']
require(type(chunks) is list and 1 <= len(chunks) <= 112)
used_fds = {header_fd, integer(script['fd'], 3, 2147483647)}
require(len(used_fds) == 2)
raw_chunks = []
total = 0
for index, chunk in enumerate(chunks):
    require(type(chunk) is dict and set(chunk) == {'index', 'fd', 'bytes'})
    require(type(chunk['index']) is int and chunk['index'] == index)
    fd = integer(chunk['fd'], 3, 2147483647)
    require(fd not in used_fds)
    used_fds.add(fd)
    size = integer(chunk['bytes'], 1, 1048576)
    total += size
    require(total <= 117440512)
    raw_chunks.append(read_exact_fd(fd, size, 1048576))
require(type(envelope) is dict and 'stream' not in envelope)
envelope['stream'] = decode(b''.join(raw_chunks))
namespace = {'__name__': 'ryeos_selected_semantic_checker'}
exec(compile(script_bytes, '<ryeos-selected-checker>', 'exec'), namespace)
require_module_origins()
response = namespace['check_conversation'](envelope)
require_module_origins()
encoded = json.dumps(response, sort_keys=True, separators=(',', ':'), ensure_ascii=True, allow_nan=False).encode('utf-8') + b'\n'
require(len(encoded) <= 65536)
sys.stdout.buffer.write(encoded)
sys.stdout.buffer.flush()
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::subordinate_process::SubordinateProcessRequest;
    use lillux::time::Duration;
    use std::sync::mpsc;

    // Exact-runtime component controls, authored and UNRUN. Resource paths only
    // locate authentic retained bytes; they never confer Tool/admission authority.
    #[cfg(target_os = "linux")]
    const CONFORMANCE_TREE: &str = "800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf";
    #[cfg(target_os = "linux")]
    const CONFORMANCE_PREFIX: &str = "/ryeos/realizations/producer-python";

    #[cfg(target_os = "linux")]
    fn verify_conformance_member(member: &lillux::PinnedRegularFile, bytes: u64, hash: &str) -> Result<()> {
        member.require_executable()?;
        ensure!(member.permission_mode()? & 0o777 == 0o755, "selected executable mode changed");
        let (actual, _) = lillux::secure_fs::digest_open_regular_file_stable_exact(&member.try_clone_descriptor()?, bytes)?;
        ensure!(actual == hash, "selected executable content changed");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[derive(Clone, Copy, Debug)]
    enum ConformanceCase { Positive, ScriptHash, ChunkSize, HeaderTruncated, DescriptorClosed, Oversized, Nonzero, Trailing, NoEof, WritableMount, SubstituteMount, ExtraFdAlias, ChunkOrder, AmbientEnvironment }

    #[cfg(target_os = "linux")]
    fn run_runtime_conformance(case: ConformanceCase) -> Result<()> {
        use std::ffi::OsString;
        use std::io::{Read as _, Write as _};
        use std::os::fd::AsFd as _;
        use std::path::{Path, PathBuf};
        use lillux::{LinuxSandboxMount, LinuxSandboxMountAccess as Access};
        let deadline = MonotonicDeadline::after(Duration::from_secs(60));
        let manifest_path = std::env::var_os("RYEOS_CHECKER_CONFORMANCE_MANIFEST").context("NOT ESTABLISHED: exact retained manifest resource absent")?;
        let runtime_path = std::env::var_os("RYEOS_CHECKER_CONFORMANCE_RUNTIME").context("NOT ESTABLISHED: exact retained runtime resource absent")?;
        let manifest_member = lillux::secure_fs::open_pinned_regular_file_no_follow(Path::new(&manifest_path))?;
        let manifest_observation = manifest_member.observation()?;
        let raw = manifest_member.read_stable_bounded(&manifest_observation, 2 * 1024 * 1024)?;
        let manifest_source = manifest_member.inherited_descriptor_authority()?;
        ensure!(lillux::sha256_hex(&raw) == CONFORMANCE_TREE, "runtime manifest resource identity changed");
        let manifest = ryeos_state::objects::ExternalContentManifestObject::from_value(&serde_json::from_slice(&raw)?)?;
        ensure!(manifest.entry_count == 4480 && manifest.total_bytes == 68732630, "runtime manifest closure bounds changed");
        let root = lillux::PinnedDirectory::open(Path::new(&runtime_path))?.context("runtime locator absent")?;
        let runtime_source = root.inherited_descriptor_authority()?;
        let verify_root = |source: &InheritedDescriptorAuthority| {
            ryeos_state::external_content::realization_verification::verify_staged_external_realization(
                source, &manifest_source, ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
                CONFORMANCE_TREE, raw.len() as u64, ryeos_state::objects::ExternalContentKind::Tree, 68732630)
        };
        verify_root(&runtime_source)?;
        if matches!(case, ConformanceCase::SubstituteMount) {
            let substitute = tempfile::tempdir()?;
            let substitute_root = lillux::PinnedDirectory::open(substitute.path())?.context("substitute absent")?;
            let substitute_source = substitute_root.inherited_descriptor_authority()?;
            ensure!(verify_root(&substitute_source).is_err(), "substitute runtime unexpectedly verified");
            bail!("selected substitute closure refused");
        }
        let loader = root.open_pinned_regular_descendant(Path::new("lib/ld-musl-x86_64.so.1"), false)?.context("loader absent")?;
        let interpreter = root.open_pinned_regular_descendant(Path::new("python/bin/python3.14"), false)?.context("interpreter absent")?;
        verify_conformance_member(&loader, 662120, "1cc5c3f502179e2a4befae29658fb109102effefc75742893aa4a449fd3fbb03")?;
        verify_conformance_member(&interpreter, 22166488, "2d8886b2bad1105014ce23e2439b20d47a78416281b906e21ac0e37085d19e9d")?;
        let runtime_mount = runtime_source.duplicate_at_or_above(256).map_err(anyhow::Error::msg)?;
        let loader_mount = loader.inherited_descriptor_authority()?.duplicate_at_or_above(256).map_err(anyhow::Error::msg)?;
        let interpreter_fd = interpreter.inherited_descriptor_authority()?.duplicate_at_or_above(256).map_err(anyhow::Error::msg)?;
        let canary = tempfile::tempdir()?;
        let canary_name = "ryeos_unselected_host_canary_c4ea728d";
        let canary_path = canary.path().join(format!("{canary_name}.py"));
        std::fs::write(&canary_path, b"raise RuntimeError('ambient import')")?;
        // Test-local transport probe only; no captured checker-code witness.
        let mut script = br#"def check_conversation(envelope):
    import os
    if envelope['probe'] != 'runtime-only' or envelope['stream'] != {'probe': 1}:
        raise ValueError('transport input changed')
    if os.path.exists(envelope['canary']) or any(os.path.exists(p) for p in ('/usr', '/bin', '/lib', '/lib64')):
        raise ValueError('ambient filesystem exposed')
    try:
        __import__(envelope['canary_name'])
    except ModuleNotFoundError:
        pass
    else:
        raise ValueError('ambient Python import succeeded')
    try:
        with open('/ryeos/realizations/producer-python/lib/ld-musl-x86_64.so.1', 'wb'):
            pass
    except OSError:
        pass
    else:
        raise ValueError('runtime writable')
    return {'schema':'ryeos.test.checker_transport.v1','ok':True}
"#.to_vec();
        match case {
            ConformanceCase::Oversized => script.extend_from_slice(b"\ndef check_conversation(envelope): return {'overflow':'x'*65536}\n"),
            ConformanceCase::Nonzero => script.extend_from_slice(b"\nraise RuntimeError('test refusal')\n"),
            ConformanceCase::Trailing => script.extend_from_slice(b"\nprint('unselected-frame')\n"),
            ConformanceCase::NoEof => script.extend_from_slice(b"\ndef check_conversation(envelope):\n    import sys\n    sys.stdout.write('{\"ok\":true,\"schema\":\"ryeos.test.checker_transport.v1\"}\\n')\n    sys.stdout.flush()\n    while True: pass\n"),
            _ => {}
        }
        let script_hash = lillux::sha256_hex(&script);
        let script_fd = lillux::exec::sealed_memfd(c"conformance-script", &script).map_err(anyhow::Error::msg)?;
        let chunk_fd = lillux::exec::sealed_memfd(c"conformance-chunk", br#"{"probe":1}"#).map_err(anyhow::Error::msg)?;
        // Source owners are all >=256; mapped closed role table uses 100..103.
        let owners = vec![interpreter_fd, script_fd.duplicate_at_or_above(256).map_err(anyhow::Error::msg)?, chunk_fd.duplicate_at_or_above(256).map_err(anyhow::Error::msg)?];
        let hash = if matches!(case, ConformanceCase::ScriptHash) { "0".repeat(64) } else { script_hash };
        let mut table = DescriptorTable { schema: TABLE_SCHEMA, script: ScriptDescriptor { fd:101, bytes:script.len() as u64, sha256:&hash },
            chunks:vec![ChunkDescriptor { index:0, fd:102, bytes:if matches!(case, ConformanceCase::ChunkSize) { 12 } else { 11 } }] };
        if matches!(case, ConformanceCase::ChunkOrder) { table.chunks[0].index = 1; }
        let metadata = lillux::canonical_json(&serde_json::json!({"probe":"runtime-only", "canary":canary_path.to_str().context("canary path UTF8")?, "canary_name":canary_name}))?;
        let size = table_encoded_size(&table)?;
        let total = combined_header_size(metadata.len(), size, MAX_HEADER_BYTES)?;
        let table_bytes = lillux::canonical_json(&serde_json::to_value(&table)?)?;
        let mut header = Vec::with_capacity(total);
        header.extend_from_slice(HEADER_MAGIC); header.extend_from_slice(&u32::try_from(metadata.len())?.to_be_bytes()); header.extend_from_slice(metadata.as_bytes());
        header.extend_from_slice(&u32::try_from(table_bytes.len())?.to_be_bytes()); header.extend_from_slice(table_bytes.as_bytes());
        ensure!(header.len() == total, "header preflight differs");
        if matches!(case, ConformanceCase::HeaderTruncated) { header.pop(); }
        let header_fd = lillux::exec::sealed_memfd(c"conformance-header", &header).map_err(anyhow::Error::msg)?.duplicate_at_or_above(256).map_err(anyhow::Error::msg)?;
        let mut channels = owners.iter().zip([100,101,102]).map(|(o,t)| o.inherited_descriptor().map(|s| (s,t))).collect::<std::result::Result<Vec<_>,_>>().map_err(anyhow::Error::msg)?;
        if matches!(case, ConformanceCase::DescriptorClosed) { channels.retain(|(_, target)| *target != 102); }
        channels.push((header_fd.inherited_descriptor().map_err(anyhow::Error::msg)?,103));
        let executable = PathBuf::from(format!("{CONFORMANCE_PREFIX}/lib/ld-musl-x86_64.so.1"));
        if matches!(case, ConformanceCase::ExtraFdAlias) { channels[1].1 = 100; }
        let mut request = lillux::LinuxSandboxRequest {
            executable:executable.clone(), argv0:OsString::from("ryeos-admitted-checker-loader"),
            arguments:vec!["--library-path".into(), format!("{CONFORMANCE_PREFIX}/lib").into(), "/proc/self/fd/100".into(), "-I".into(), "-S".into(), "-B".into(), "-X".into(), "utf8".into(), "-u".into(), "-c".into(), TRANSPORT_BOOTSTRAP.into(), format!("{CONFORMANCE_PREFIX}/python").into()],
            cwd:CONFORMANCE_PREFIX.into(), environment:Default::default(),
            mounts:vec![LinuxSandboxMount {source_fd:runtime_mount.inherited_descriptor().map_err(anyhow::Error::msg)?,destination:CONFORMANCE_PREFIX.into(),access:Access::ReadOnly,layer:0},
                LinuxSandboxMount {source_fd:loader_mount.inherited_descriptor().map_err(anyhow::Error::msg)?,destination:executable,access:Access::ReadOnly,layer:20}],
            fixed_parent_views:vec![],overlay:None,network:lillux::LinuxSandboxNetwork::Isolated,private_tmp:true,
            proc_filesystem:lillux::LinuxSandboxProcFilesystem::PidNamespace,minimal_devices:true,character_devices:vec![],target_channels:channels,
            lifecycle:lillux::LinuxSandboxLifecycle::Run,contain_process_group:true,nested_sandbox:false,aggregate_limits:None,
            target_limits:Some(SubprocessLimits {deny_process_creation:true,..Default::default()}),
        };
        if matches!(case, ConformanceCase::AmbientEnvironment) {
            // Adversarial component control, not production environment data.
            request.environment.insert("PYTHONPATH".into(), canary.path().as_os_str().into());
            request.environment.insert("PYTHONHOME".into(), canary.path().as_os_str().into());
        }
        let expected_mounts = lillux::LinuxSandboxMountPreparationCommitments::from_admitted_mounts(&request.mounts,&[]).map_err(anyhow::Error::msg)?;
        let expected_launch = lillux::LinuxSandboxAppliedLaunchCommitments::from_target(lillux::LinuxSandboxAppliedLaunchTarget {
            executable:&request.executable,argv0:&request.argv0,arguments:&request.arguments,cwd:&request.cwd,environment:&request.environment }).map_err(anyhow::Error::msg)?;
        if matches!(case, ConformanceCase::WritableMount) { request.mounts[0].access = Access::Writable; }
        let (mut held,pipes) = match lillux::prepare_linux_sandbox_piped(request) {
            Ok(value) => value,
            Err(error) if matches!(case, ConformanceCase::ExtraFdAlias)
                && error.contains("uniquely target-fd sorted") => bail!("selected descriptor alias refused"),
            Err(error) => return Err(anyhow::Error::msg(error)),
        };
        let result = (|| -> Result<()> {
            let observed = held.mount_preparation_receipt().map_err(anyhow::Error::msg)?;
            ensure!(observed.owned_child_pid == held.child_pid() && observed.matches_commitments(&expected_mounts), "held runtime mounts differ");
            verify_conformance_member(&loader,662120,"1cc5c3f502179e2a4befae29658fb109102effefc75742893aa4a449fd3fbb03")?;
            verify_conformance_member(&interpreter,22166488,"2d8886b2bad1105014ce23e2439b20d47a78416281b906e21ac0e37085d19e9d")?;
            verify_root(&runtime_source)?;
            ensure!(!deadline.has_elapsed(),"runtime preparation original deadline expired");
            held.release_once().map_err(anyhow::Error::msg)?;
            let mut input = lillux::exec::DeadlinePipeStream::new(pipes.stdin.as_fd(),deadline)?;
            input.write_all(b"103\n")?;
            drop(input);
            // Bootstrap consumes stdin EOF before opening the sealed header.
            drop(pipes.stdin);
            let mut output = lillux::exec::DeadlinePipeStream::new(pipes.stdout.as_fd(),deadline)?;
            let mut diagnostics = lillux::exec::DeadlinePipeStream::new(pipes.stderr.as_fd(),deadline)?;
            // Both pipes are serviced in one nonblocking bounded loop: a noisy
            // refusal cannot block stdout behind a full diagnostic pipe.
            let mut bytes = Vec::new(); let mut out_eof=false; let mut err_eof=false;
            while !out_eof || !err_eof {
                ensure!(!deadline.has_elapsed(), "runtime pipe original deadline expired");
                let (out_ready, err_ready) = if !out_eof && !err_eof {
                    let ready = output.wait_readable_with_stream(&diagnostics)?;
                    (ready.channel_readable(), ready.auxiliary_readable())
                } else { (!out_eof, !err_eof) };
                for (stream,is_output,eof,ready) in [(&mut output,true,&mut out_eof,out_ready),(&mut diagnostics,false,&mut err_eof,err_ready)] {
                    if *eof || !ready {continue;}
                    let mut buf=[0u8;1024];
                    match stream.read(&mut buf) {
                        Ok(0)=>*eof=true,
                        Ok(n)=>if is_output {ensure!(bytes.len()+n<=MAX_RESPONSE_BYTES,"runtime response overflow");bytes.extend_from_slice(&buf[..n]);},
                        Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>{},
                        Err(e)=>return Err(e.into()),
                    }
                }
            }
            let applied = loop {
                if let Some(receipt)=held.try_observe_applied_launch().map_err(anyhow::Error::msg)? {break receipt;}
                ensure!(!deadline.has_elapsed(),"applied launch original deadline expired");std::thread::yield_now();
            };
            ensure!(applied.owned_child_pid==held.child_pid() && applied.matches_commitments(&expected_launch) && applied.matches_post_release_mounts(&expected_mounts),"runtime applied launch differs");
            let termination = loop {
                if let Some(exit)=held.try_observe_target_exit().map_err(anyhow::Error::msg)? {break exit.into_termination();}
                ensure!(!deadline.has_elapsed(),"namespace reap original deadline expired");std::thread::yield_now();
            };
            ensure!(termination.exit()==lillux::LinuxSandboxExit::Code(0) && termination.launch_failure().is_none(),"runtime transport refused");
            ensure!(held.child_pid()==0,"namespace owner not consumed");
            ensure!(bytes==b"{\"ok\":true,\"schema\":\"ryeos.test.checker_transport.v1\"}\n","runtime response not exact single frame");
            Ok(())
        })();
        if result.is_err() {
            let _bounded_cleanup=held.terminate_namespace_for_export_until(deadline);
            // Best-effort terminal TEST cleanup may block/fail, never proves
            // settlement or renews service. All source/pipe owners outlive it.
            drop(held);
        }
        let _ = (&owners, &header_fd, &runtime_mount, &loader_mount, &root, &loader, &interpreter, &canary, &manifest_source, &manifest_member);
        result.context("actual runtime attempt refused")
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "actual retained producer-Python resources and Linux namespaces required; source-only control"]
    fn authentic_python_fd_bootstrap_and_readonly_enclosure() {
        run_runtime_conformance(ConformanceCase::Positive).expect("NOT ESTABLISHED: authentic runtime conformance failed");
        run_runtime_conformance(ConformanceCase::AmbientEnvironment).expect("NOT ESTABLISHED: hostile Python environment affected isolated bootstrap");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "real-runtime refusal controls; UNRUN, not checker-code qualification"]
    fn authentic_python_transport_substitutions_refuse() {
        // Establish the same real runtime first, so missing resources/startup
        // cannot make every negative case appear to pass vacuously.
        run_runtime_conformance(ConformanceCase::Positive).expect("NOT ESTABLISHED: positive runtime baseline absent");
        for case in [ConformanceCase::ScriptHash,ConformanceCase::ChunkSize,ConformanceCase::HeaderTruncated,
            ConformanceCase::DescriptorClosed,ConformanceCase::Oversized,ConformanceCase::Nonzero,ConformanceCase::Trailing,ConformanceCase::NoEof,ConformanceCase::WritableMount,
            ConformanceCase::SubstituteMount,ConformanceCase::ExtraFdAlias,ConformanceCase::ChunkOrder] {
            let error = run_runtime_conformance(case).expect_err("runtime refused-control accepted");
            assert!(error.to_string().contains("actual runtime attempt refused")
                || (matches!(case, ConformanceCase::SubstituteMount) && error.to_string()=="selected substitute closure refused")
                || (matches!(case, ConformanceCase::ExtraFdAlias) && error.to_string()=="selected descriptor alias refused"),
                "control failed before actual runtime attempt, not refusal evidence: {case:?}: {error:#}");
        }
    }

    // These controls are authored/unrun. Mechanical shell peers below are NOT
    // checker runtimes, admitted source pins, Python conformance or authority.
    fn peer(script: &str) -> SubordinateProcess {
        SubordinateProcess::spawn(SubordinateProcessRequest {
            cmd: "/bin/sh".into(),
            argv0: None,
            args: vec!["-c".into(), script.into()],
            cwd: "/".into(),
            envs: Vec::new(),
            limits: None,
            inherited_fds: Vec::new(),
        })
        .unwrap()
    }

    fn observing_peer(script: &str) -> Attempt {
        let mut state = Attempt {
            child: Some(peer(script)),
            ..Default::default()
        };
        let child = state.child.as_mut().unwrap();
        state.drain = Some(child.take_error().unwrap().start_discarding().unwrap());
        state.output = Some(child.take_output().unwrap());
        state.input = Some(child.take_input().unwrap());
        state.input.take();
        state
    }

    // Harness cleanup only, never a production/acceptance deadline renewal.
    // It reaps deliberately malformed peer controls AFTER rejecting them;
    // it never calls any semantic or success constructor.
    fn dispose_peer_control(state: &mut Attempt) {
        let deadline = MonotonicDeadline::after(Duration::from_secs(5));
        if let Some(child) = state.child.as_mut() {
            assert!(child.kill_exact_child_until(deadline).unwrap().is_some());
        }
        if let Some(drain) = state.drain.as_mut() {
            assert!(drain.cancel_until(deadline).is_some());
        }
        state.child.take();
        state.drain.take();
    }

    #[test]
    fn total_header_preflight_charges_table_and_framing_at_exact_boundary() {
        let hash = "a".repeat(64);
        let table = DescriptorTable {
            schema: TABLE_SCHEMA,
            script: ScriptDescriptor {
                fd: 999,
                bytes: 33554432,
                sha256: &hash,
            },
            chunks: vec![
                ChunkDescriptor {
                    index: 0,
                    fd: 1000,
                    bytes: 1048576,
                },
                ChunkDescriptor {
                    index: 1,
                    fd: 10001,
                    bytes: 1,
                },
            ],
        };
        let observed = lillux::canonical_json(&serde_json::to_value(&table).unwrap()).unwrap();
        let size = table_encoded_size(&table).unwrap();
        assert_eq!(observed.len(), size);
        let overhead = 17 + size;
        assert_eq!(
            combined_header_size(MAX_HEADER_BYTES - overhead, size, MAX_HEADER_BYTES).unwrap(),
            MAX_HEADER_BYTES
        );
        assert!(
            combined_header_size(MAX_HEADER_BYTES - overhead + 1, size, MAX_HEADER_BYTES).is_err()
        );
        assert!(combined_header_size(MAX_HEADER_BYTES, size, MAX_HEADER_BYTES).is_err());
        assert!(combined_header_size(usize::MAX, size, MAX_HEADER_BYTES).is_err());
    }

    #[test]
    fn one_response_requires_real_eof_zero_exit_exact_reap_and_diagnostic_join() {
        let cancellation = Cancellation::default();
        let mut state = observing_peer("printf '{}\\n'");
        let result = read_and_settle_checker(
            &mut state,
            &cancellation,
            MonotonicDeadline::after(Duration::from_secs(5)),
        );
        dispose_peer_control(&mut state);
        assert_eq!(result.unwrap(), b"{}\n");
        // Local framing success is insufficient for semantic/verified success:
        // P's five-identity closed decoder is mandatory in run_transaction.
    }

    #[test]
    fn trailing_bytes_oversize_and_nonzero_exit_cannot_supply_a_local_join() {
        for script in [
            "printf '{}\\nextra'",
            "printf '{}\\n'; exit 7",
            "i=0; while [ $i -lt 65537 ]; do printf x; i=$((i+1)); done",
        ] {
            let mut state = observing_peer(script);
            let result = read_and_settle_checker(
                &mut state,
                &Cancellation::default(),
                MonotonicDeadline::after(Duration::from_secs(5)),
            );
            dispose_peer_control(&mut state);
            assert!(
                result.is_err(),
                "malformed peer unexpectedly supplied terminal join"
            );
        }
    }

    #[test]
    fn response_without_eof_expires_on_the_original_deadline_and_retains_child() {
        // Busy direct peer avoids spawning an unowned test descendant.
        let mut state = observing_peer("printf '{}\\n'; while :; do :; done");
        let deadline = MonotonicDeadline::after(Duration::from_millis(40));
        let result = read_and_settle_checker(&mut state, &Cancellation::default(), deadline);
        let retained = state.child.is_some() && state.output.is_some() && state.drain.is_some();
        dispose_peer_control(&mut state);
        assert!(result.is_err() && deadline.has_elapsed() && retained);
    }

    #[test]
    fn panic_before_and_after_spawn_preserves_custody_and_only_refuses() {
        for after_spawn in [false, true] {
            let shared = SharedAttempt::new();
            shared.locally_settled.store(false, Ordering::Release);
            let result = run_owned_transaction(
                &shared,
                MonotonicDeadline::after(Duration::from_secs(5)),
                |state, _| {
                    if after_spawn {
                        state.child = Some(peer("while :; do :; done"));
                    }
                    panic!("injected owned-worker control");
                },
            );
            assert!(result.is_err());
            assert!(shared.locally_settled.load(Ordering::Acquire));
            assert!(shared.state.lock().unwrap().child.is_none());
        }
    }

    #[test]
    fn cancellation_racing_token_publication_is_an_exact_request_not_success() {
        let cancellation = Cancellation::default();
        cancellation.request(); // request arrives BEFORE a retained child token
        let mut state = observing_peer("while :; do :; done");
        cancellation
            .publish(
                state
                    .child
                    .as_ref()
                    .unwrap()
                    .cooperative_termination()
                    .unwrap(),
            )
            .unwrap();
        assert!(cancellation.tokens.lock().unwrap().pending.is_some());
        assert!(
            cancellation
                .require_running(MonotonicDeadline::after(Duration::from_secs(5)))
                .is_err()
        );
        dispose_peer_control(&mut state);
    }

    #[test]
    fn parent_terminal_conversion_never_acquires_worker_io_guard_or_joins_live_task() {
        let mut owner = CheckerInvocationOwner::default();
        owner.started = true;
        owner.shared.locally_settled.store(false, Ordering::Release);
        let shared = Arc::clone(&owner.shared);
        let (held_send, held_recv) = mpsc::channel();
        let (release_send, release_recv) = mpsc::channel();
        owner.task = Some(
            lillux::task::spawn_host_task("checker-held-io-control", move || {
                let _guard = shared.state.lock().unwrap();
                held_send.send(()).unwrap();
                release_recv.recv().unwrap();
                shared.locally_settled.store(true, Ordering::Release);
                Err(anyhow::anyhow!("control released; no success"))
            })
            .unwrap(),
        );
        held_recv
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        // If parent acquires state/joins the live task, this call cannot finish
        // because release is deliberately sent only AFTER terminal transfer.
        let mut failure = owner
            .into_terminal_failure()
            .expect("live task must stay owned");
        assert!(failure._task.is_some());
        assert!(
            failure
                ._shared
                .cancellation
                .requested
                .load(Ordering::Acquire)
        );
        release_send.send(()).unwrap();
        let task = failure._task.take().unwrap();
        match task.join_until(MonotonicDeadline::after(Duration::from_secs(5))) {
            Ok(outcome) => assert!(outcome.unwrap().is_err()),
            Err(task) => {
                failure._task = Some(task);
                panic!("control task did not settle");
            }
        }
    }

    #[test]
    fn poisoned_attempt_is_cleanup_only_and_cannot_manufacture_success() {
        let shared = SharedAttempt::new();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = shared.state.lock().unwrap();
            panic!("injected state poison");
        }));
        let called = AtomicBool::new(false);
        let result = run_owned_transaction(
            &shared,
            MonotonicDeadline::after(Duration::from_secs(5)),
            |_, _| {
                called.store(true, Ordering::Release);
                bail!("unreachable poisoned transaction");
            },
        );
        assert!(result.is_err() && !called.load(Ordering::Acquire));
        assert!(shared.locally_settled.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn dropped_waiter_requests_cancellation_while_root_retains_the_worker() {
        let mut owner = CheckerInvocationOwner::default();
        owner.started = true;
        owner.shared.locally_settled.store(false, Ordering::Release);
        let shared = Arc::clone(&owner.shared);
        let deadline = MonotonicDeadline::after(Duration::from_secs(5));
        let (held_send, held_recv) = mpsc::channel();
        let (release_send, release_recv) = mpsc::channel();
        owner.task = Some(
            lillux::task::spawn_host_task("checker-dropped-waiter-control", move || {
                let mut state = shared.state.lock().unwrap();
                state.child = Some(peer("while :; do :; done"));
                shared
                    .cancellation
                    .publish(
                        state
                            .child
                            .as_ref()
                            .unwrap()
                            .cooperative_termination()
                            .unwrap(),
                    )
                    .unwrap();
                held_send.send(()).unwrap();
                release_recv.recv().unwrap();
                let settled = state.settle_refusal(deadline);
                shared.locally_settled.store(settled, Ordering::Release);
                Err(anyhow::anyhow!("cancelled mechanical control; no success"))
            })
            .unwrap(),
        );
        held_recv
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let expired = tokio::time::timeout(
            std::time::Duration::from_millis(5),
            owner.await_task(deadline),
        )
        .await;
        assert!(expired.is_err());
        assert!(owner.shared.cancellation.requested.load(Ordering::Acquire));
        assert!(owner.task.is_some());
        assert!(
            owner
                .shared
                .cancellation
                .tokens
                .lock()
                .unwrap()
                .pending
                .is_some()
        );
        let mut failure = owner
            .into_terminal_failure()
            .expect("live cancelled worker retained");
        release_send.send(()).unwrap();
        let task = failure._task.take().unwrap();
        match task.join_until(deadline) {
            Ok(outcome) => assert!(outcome.unwrap().is_err()),
            Err(task) => {
                failure._task = Some(task);
                panic!("cancelled control did not settle");
            }
        }
        assert!(failure._shared.locally_settled.load(Ordering::Acquire));
    }

    #[test]
    fn startup_write_failure_keeps_exact_child_and_pipe_owners() {
        // Keep a dedicated writer across exact child reap; broken input is
        // refusal with retained ownership, not successful checker output.
        let mut state = Attempt {
            child: Some(peer("exit 0")),
            ..Default::default()
        };
        state.input = Some(state.child.as_mut().unwrap().take_input().unwrap());
        let deadline = MonotonicDeadline::after(Duration::from_secs(5));
        state
            .child
            .as_mut()
            .unwrap()
            .wait_exact_child_until(deadline)
            .unwrap()
            .unwrap();
        let result = state
            .input
            .as_mut()
            .unwrap()
            .write_all_until(b"123\n", deadline);
        let retained = state.child.is_some() && state.input.is_some();
        dispose_peer_control(&mut state);
        assert!(result.is_err() && retained);
    }
}
