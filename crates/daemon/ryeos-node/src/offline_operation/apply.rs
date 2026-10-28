//! Application of offline operations.
//!
//! One library function, [`apply_pending_offline_operations`], runs at the
//! very top of the daemon's startup, before any check that could refuse, and
//! applies what the inbox holds. The command-line tool applies its own signed
//! requests in process through [`apply_local`], which is the same code path
//! with the same checks, the same ledger and the same receipts.
//!
//! It needs only a minimal verified state, each part read from the volume
//! with the code the node already uses: the node identity, the trust store,
//! the node-signed grants, the signed init completion when present, and the
//! ledger. It takes the state lock itself and releases it before normal
//! startup continues.

mod ledger;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use ryeos_engine::roots::RuntimeRoot;
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_node_runtime::state_lock::{StateLock, default_lock_path};
use serde_json::{Value, json};

pub use ledger::{LEDGER_KEEP, LEDGER_ROTATE_AT, Ledger, LedgerState};

use super::contract::{
    DEFAULT_REQUEST_LIFETIME, REQUEST_SCHEMA, Refusal, RefusalReason, RequestBody, SignedReceipt,
    SignedRequest,
};
use super::envelope::{Envelope, WildcardGrant, wildcard_only_grants};
use super::facts::{FACT_LEDGER_HEAD, MANDATORY_FACTS, NodeFacts, measure};
use super::host::OfflineHost;
use super::store::{Pending, Phase, Read, Store};
use super::table::{
    Handler, HandlerContext, OperationSpec, VerifiedInputs, lookup, validate_table,
};
use ledger::ReceiptDraft;

/// A request whose handler fails, or whose node dies inside it, this many
/// startups running is marked stuck and refused.
pub const MAX_STARTUP_ATTEMPTS: u32 = 3;
/// Application stops at the next request boundary once this has passed.
pub const DEFAULT_TIME_BUDGET: Duration = Duration::from_secs(15 * 60);
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounds how many times the inbox is re-read in one startup.
const MAX_ROUNDS: usize = 64;

pub struct ApplyOptions {
    pub time_budget: Duration,
    pub lock_timeout: Duration,
    pub clock: fn() -> DateTime<Utc>,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            time_budget: DEFAULT_TIME_BUDGET,
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
            clock: Utc::now,
        }
    }
}

/// Why application did nothing at all. Startup continues in every case: the
/// ordinary checks that follow report a node that cannot start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skipped {
    NotInitialized,
    NodeIdentityUnreadable(String),
    LockHeld(String),
    LedgerUnreadable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// An authority-bearing operation was applied; the startup proceeds with
    /// the new state and further requests wait for the next one.
    AuthorityBearingApplied(String),
    /// An operation that must be applied alone was ready behind another.
    AloneDeferred(String),
    TimeBudget,
    HandlerFailed {
        operation: String,
        request_digest: String,
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedOperation {
    pub request_digest: String,
    pub operation: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedOperation {
    pub request_digest: String,
    pub reason: RefusalReason,
}

#[derive(Debug, Default)]
pub struct ApplyReport {
    pub skipped: Option<Skipped>,
    /// The node's precondition facts as measured at the start.
    pub facts: NodeFacts,
    pub wildcard_grants: Vec<WildcardGrant>,
    pub applied: Vec<AppliedOperation>,
    pub refused: Vec<RefusedOperation>,
    /// Requests left in the inbox for a later startup.
    pub waiting: usize,
    pub stopped: Option<StopReason>,
}

impl ApplyReport {
    /// Lines for the startup log. The facts are printed on every startup so a
    /// node that crash-loops still tells its operator what to sign against.
    pub fn log_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(skipped) = &self.skipped {
            lines.push(format!("offline-operations: not applied: {skipped:?}"));
        }
        for (fact, value) in &self.facts {
            lines.push(format!("offline-operations: fact {fact}={value}"));
        }
        for grant in &self.wildcard_grants {
            lines.push(format!(
                "offline-operations: grant {} would match {} under wildcard rules; wildcards never satisfy an offline scope",
                grant.fingerprint,
                grant.operations.join(", ")
            ));
        }
        for applied in &self.applied {
            lines.push(format!(
                "offline-operations: applied {} request {} as receipt {}",
                applied.operation, applied.request_digest, applied.sequence
            ));
        }
        for refused in &self.refused {
            lines.push(format!(
                "offline-operations: refused request {}: {}",
                refused.request_digest,
                refused.reason.code()
            ));
        }
        if self.waiting > 0 {
            lines.push(format!(
                "offline-operations: {} request(s) wait for a later startup",
                self.waiting
            ));
        }
        if let Some(stopped) = &self.stopped {
            lines.push(format!("offline-operations: stopped: {stopped:?}"));
        }
        lines
    }
}

fn node_identity(app_root: &Path) -> Result<Option<NodeIdentity>, String> {
    let path = RuntimeRoot::new(app_root.to_path_buf()).node_signing_key_path();
    match lillux::read_optional_regular_file_bounded_no_follow(&path, 64 * 1024) {
        Ok(None) => Ok(None),
        Ok(Some(_)) => NodeIdentity::load(&path)
            .map(Some)
            .map_err(|error| format!("{error:#}")),
        Err(error) => Err(format!("{error:#}")),
    }
}

/// Apply what the inbox holds, at the top of daemon startup.
pub fn apply_pending_offline_operations(
    app_root: &Path,
    host: &dyn OfflineHost,
) -> Result<ApplyReport> {
    apply_with_options(app_root, host, &ApplyOptions::default())
}

pub fn apply_with_options(
    app_root: &Path,
    host: &dyn OfflineHost,
    options: &ApplyOptions,
) -> Result<ApplyReport> {
    validate_table().context("the compiled operation table is invalid")?;
    let mut report = ApplyReport::default();
    let node = match node_identity(app_root) {
        Ok(Some(node)) => node,
        Ok(None) => {
            report.skipped = Some(Skipped::NotInitialized);
            return Ok(report);
        }
        Err(error) => {
            report.skipped = Some(Skipped::NodeIdentityUnreadable(error));
            return Ok(report);
        }
    };
    let state_dir = RuntimeRoot::new(app_root.to_path_buf()).state();
    if lillux::PinnedDirectory::open(&state_dir)?.is_none() {
        report.skipped = Some(Skipped::NotInitialized);
        return Ok(report);
    }
    let lock =
        match StateLock::acquire_with_timeout(&default_lock_path(app_root), options.lock_timeout) {
            Ok(lock) => lock,
            Err(error) => {
                report.skipped = Some(Skipped::LockHeld(format!("{error:#}")));
                return Ok(report);
            }
        };
    lock.ensure_protects_app_root(app_root)?;
    let store = Store::for_app_root(app_root);
    store.create_layout()?;
    let mut state = match Ledger::new(&store, &node).load() {
        Ok(state) => state,
        Err(error) => {
            report.skipped = Some(Skipped::LedgerUnreadable(format!("{error:#}")));
            report.facts = measure(app_root, "unreadable");
            return Ok(report);
        }
    };
    report.facts = measure(app_root, &state.head);
    report.wildcard_grants = wildcard_only_grants(app_root, &node);
    let applier = Applier {
        app_root,
        host,
        node: &node,
        store: &store,
        lock: &lock,
        clock: options.clock,
    };
    let started = Instant::now();
    let mut run = Run {
        report: &mut report,
        started,
        budget: options.time_budget,
    };
    applier.run_startup(&mut state, &mut run)?;
    Ok(report)
}

// ── The applier ──────────────────────────────────────────────────────────

struct Applier<'a> {
    app_root: &'a Path,
    host: &'a dyn OfflineHost,
    node: &'a NodeIdentity,
    store: &'a Store,
    lock: &'a StateLock,
    clock: fn() -> DateTime<Utc>,
}

struct Run<'a> {
    report: &'a mut ApplyReport,
    started: Instant,
    budget: Duration,
}

impl Run<'_> {
    fn out_of_time(&self) -> bool {
        self.started.elapsed() >= self.budget
    }
}

/// Where a request's inputs come from.
enum InputSource<'a> {
    Inbox,
    Memory(&'a BTreeMap<String, Vec<u8>>),
}

/// How an attempt to apply a request ended.
enum Attempt {
    Applied {
        sequence: u64,
        operation: &'static str,
        authority_bearing: bool,
    },
    Refused(Refusal),
    /// The handler failed; the request stays for another startup.
    Failed(anyhow::Error),
    /// The request must be applied alone and something came before it.
    Deferred,
}

/// What evaluating an inbox candidate decided.
enum Verdict {
    Ready,
    Refuse(Refusal),
    /// Signed against a head the ledger has not reached.
    Wait,
}

struct Candidate {
    name: String,
    bytes: Vec<u8>,
    request: SignedRequest,
    digest: String,
}

impl Applier<'_> {
    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    fn envelope(&self) -> Envelope<'_> {
        Envelope {
            app_root: self.app_root,
            node: self.node,
            now: self.now(),
        }
    }

    fn ledger(&self) -> Ledger<'_> {
        Ledger::new(self.store, self.node)
    }

    // ── Startup ──────────────────────────────────────────────────────────

    fn run_startup(&self, state: &mut LedgerState, run: &mut Run<'_>) -> Result<()> {
        self.resume_pending(state, run)?;
        if run.report.stopped.is_some() {
            return Ok(());
        }
        for _ in 0..MAX_ROUNDS {
            if run.out_of_time() {
                run.report.stopped = Some(StopReason::TimeBudget);
                break;
            }
            let inbox = self.store.list_inbox()?;
            let mut candidates = Vec::new();
            let mut refused_any = false;
            for name in &inbox.requests {
                match self.load_candidate(name, run)? {
                    Some(candidate) => candidates.push(candidate),
                    None => refused_any = true,
                }
            }
            let mut ready: Vec<&Candidate> = Vec::new();
            let mut waiting = 0;
            for candidate in &candidates {
                match self.evaluate(state, candidate) {
                    Verdict::Ready => ready.push(candidate),
                    Verdict::Wait => waiting += 1,
                    Verdict::Refuse(refusal) => {
                        self.refuse_candidate(candidate, &refusal, run)?;
                        refused_any = true;
                    }
                }
            }
            run.report.waiting = waiting + usize::from(inbox.overflow);
            // Of requests signed against the same head at most one applies:
            // the earliest issued, with the digest breaking a tie.
            ready.sort_by(|a, b| {
                a.request
                    .body
                    .issued_at
                    .cmp(&b.request.body.issued_at)
                    .then_with(|| a.digest.cmp(&b.digest))
            });
            let Some(next) = ready.first() else {
                if refused_any && inbox.overflow {
                    continue;
                }
                break;
            };
            match self.attempt(
                state,
                next,
                &InputSource::Inbox,
                None,
                run.report.applied.len(),
            )? {
                Attempt::Applied {
                    sequence,
                    operation,
                    authority_bearing,
                } => {
                    run.report.applied.push(AppliedOperation {
                        request_digest: next.digest.clone(),
                        operation: operation.to_owned(),
                        sequence,
                    });
                    if authority_bearing {
                        run.report.stopped =
                            Some(StopReason::AuthorityBearingApplied(operation.to_owned()));
                        break;
                    }
                }
                Attempt::Refused(refusal) => {
                    self.refuse_candidate(next, &refusal, run)?;
                }
                Attempt::Failed(error) => {
                    run.report.stopped = Some(StopReason::HandlerFailed {
                        operation: next.request.body.operation.clone(),
                        request_digest: next.digest.clone(),
                        error: format!("{error:#}"),
                    });
                    break;
                }
                Attempt::Deferred => {
                    run.report.stopped = Some(StopReason::AloneDeferred(
                        next.request.body.operation.clone(),
                    ));
                    break;
                }
            }
            *state = self.ledger().load()?;
        }
        Ok(())
    }

    /// Requests with an intent record are continued before anything new:
    /// they were begun at an earlier startup and are owed their receipt. A
    /// continuation is of an application that passed the envelope when it
    /// began, so it is not checked against the clock again: finishing what a
    /// valid authorization started is better than leaving it half made.
    fn resume_pending(&self, state: &mut LedgerState, run: &mut Run<'_>) -> Result<()> {
        for digest in self.store.pending_digests()? {
            let Some(pending) = self.store.load_pending(&digest)? else {
                continue;
            };
            let candidate = Candidate {
                name: pending.inbox_name.clone(),
                bytes: pending.request.to_bytes()?,
                digest: digest.clone(),
                request: pending.request.clone(),
            };
            if state.request_digests.contains(&digest) && pending.phase != Phase::Receipted {
                // The receipt is in the ledger: only the clean-up was owed.
                self.finish(&candidate)?;
                continue;
            }
            match self.attempt(
                state,
                &candidate,
                &InputSource::Inbox,
                Some(pending),
                run.report.applied.len(),
            )? {
                Attempt::Applied {
                    sequence,
                    operation,
                    authority_bearing,
                } => {
                    run.report.applied.push(AppliedOperation {
                        request_digest: digest,
                        operation: operation.to_owned(),
                        sequence,
                    });
                    *state = self.ledger().load()?;
                    if authority_bearing {
                        run.report.stopped =
                            Some(StopReason::AuthorityBearingApplied(operation.to_owned()));
                        return Ok(());
                    }
                }
                Attempt::Refused(refusal) => {
                    self.refuse_candidate(&candidate, &refusal, run)?;
                }
                Attempt::Failed(error) => {
                    run.report.stopped = Some(StopReason::HandlerFailed {
                        operation: candidate.request.body.operation.clone(),
                        request_digest: candidate.digest.clone(),
                        error: format!("{error:#}"),
                    });
                    return Ok(());
                }
                Attempt::Deferred => {
                    run.report.stopped = Some(StopReason::AloneDeferred(
                        candidate.request.body.operation.clone(),
                    ));
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Read and parse one inbox request. A document that cannot be read
    /// within bounds, or is not a request, is refused here (and removed);
    /// `None` says so.
    fn load_candidate(&self, name: &str, run: &mut Run<'_>) -> Result<Option<Candidate>> {
        let bytes = match self.store.read_request(name)? {
            Read::Missing => return Ok(None),
            Read::TooLarge => {
                self.refuse_unparsed(
                    name,
                    name,
                    None,
                    Refusal::new(
                        RefusalReason::Malformed,
                        "the request document exceeds its size bound",
                    ),
                    run,
                )?;
                return Ok(None);
            }
            Read::Bytes(bytes) => bytes,
        };
        let digest_of_bytes = lillux::sha256_hex(&bytes);
        if digest_of_bytes != name {
            self.refuse_unparsed(
                name,
                &digest_of_bytes,
                Some(&bytes),
                Refusal::new(
                    RefusalReason::Malformed,
                    "the request file is not named by the digest of its content",
                ),
                run,
            )?;
            return Ok(None);
        }
        match SignedRequest::parse(&bytes) {
            Ok(request) => Ok(Some(Candidate {
                name: name.to_owned(),
                digest: request.digest()?,
                bytes,
                request,
            })),
            Err(error) => {
                self.refuse_unparsed(
                    name,
                    name,
                    Some(&bytes),
                    Refusal::new(RefusalReason::Malformed, format!("{error:#}")),
                    run,
                )?;
                Ok(None)
            }
        }
    }

    /// Refuse a document that is not a usable request: one receipt under
    /// `digest`, and the inbox file `file_name` removed.
    fn refuse_unparsed(
        &self,
        file_name: &str,
        digest: &str,
        bytes: Option<&[u8]>,
        refusal: Refusal,
        run: &mut Run<'_>,
    ) -> Result<()> {
        self.store
            .record_refusal(self.node, digest, None, None, &refusal, bytes)?;
        self.store.remove_request(file_name)?;
        run.report.refused.push(RefusedOperation {
            request_digest: digest.to_owned(),
            reason: refusal.reason,
        });
        Ok(())
    }

    /// Decide what to do with a parsed request, in the order the rules state:
    /// envelope, replay, then the ledger head.
    fn evaluate(&self, state: &LedgerState, candidate: &Candidate) -> Verdict {
        let request = &candidate.request;
        if let Err(refusal) = self.envelope().check(request) {
            return Verdict::Refuse(refusal);
        }
        if state.request_digests.contains(&candidate.digest) {
            return Verdict::Refuse(Refusal::new(
                RefusalReason::Replayed,
                "the request digest is already in the ledger",
            ));
        }
        let Some(head) = request.body.precondition.get(FACT_LEDGER_HEAD) else {
            return Verdict::Refuse(Refusal::new(
                RefusalReason::PreconditionFailed,
                "the request pins no ledger head",
            ));
        };
        if *head == state.head {
            Verdict::Ready
        } else if state.is_past_head(head) {
            Verdict::Refuse(Refusal::new(
                RefusalReason::StaleLedgerHead,
                "the ledger has moved past the head the request was signed against",
            ))
        } else {
            Verdict::Wait
        }
    }

    fn refuse_candidate(
        &self,
        candidate: &Candidate,
        refusal: &Refusal,
        run: &mut Run<'_>,
    ) -> Result<()> {
        let body = &candidate.request.body;
        self.store.record_refusal(
            self.node,
            &candidate.digest,
            Some(&body.operation),
            Some(&body.signer_fingerprint),
            refusal,
            Some(&candidate.bytes),
        )?;
        self.discard(candidate)?;
        run.report.refused.push(RefusedOperation {
            request_digest: candidate.digest.clone(),
            reason: refusal.reason,
        });
        Ok(())
    }

    /// Remove a refused request, its inputs and its intent record.
    fn discard(&self, candidate: &Candidate) -> Result<()> {
        self.finish(candidate)
    }

    /// Remove an applied request and its inputs, then its intent record.
    fn finish(&self, candidate: &Candidate) -> Result<()> {
        self.store.remove_request(&candidate.name)?;
        for digest in candidate.request.body.inputs.values() {
            self.store.remove_input(digest)?;
        }
        self.store.remove_pending(&candidate.digest)
    }

    // ── Applying one request ─────────────────────────────────────────────

    /// Steps 3 to 8 for a request the envelope admitted: preconditions,
    /// inputs, the handler, the receipt, the clean-up.
    ///
    /// With an intent record the request is a continuation: its preconditions
    /// were verified when it began, and the state they described may already
    /// have been changed by the handler.
    fn attempt(
        &self,
        state: &LedgerState,
        candidate: &Candidate,
        source: &InputSource<'_>,
        resumed: Option<Pending>,
        applied_this_startup: usize,
    ) -> Result<Attempt> {
        let body = &candidate.request.body;
        let spec = lookup(&body.operation)
            .with_context(|| format!("this build has no offline operation `{}`", body.operation))?;
        if spec.alone && applied_this_startup > 0 && resumed.is_none() {
            return Ok(Attempt::Deferred);
        }
        // A continuation that has already used its attempts is stuck.
        if let Some(pending) = &resumed
            && pending.attempts >= MAX_STARTUP_ATTEMPTS
            && pending.phase == Phase::Running
        {
            return Ok(Attempt::Refused(Refusal::new(
                RefusalReason::Stuck,
                format!(
                    "the request began {} startups without finishing",
                    pending.attempts
                ),
            )));
        }
        let measured = match &resumed {
            Some(pending) => pending.measured_preconditions.clone(),
            None => match self.check_preconditions(spec, body, &state.head) {
                Ok(measured) => measured,
                Err(refusal) => return Ok(Attempt::Refused(refusal)),
            },
        };
        // A continuation that already has its result needs no inputs again.
        let inputs = if resumed
            .as_ref()
            .is_some_and(|pending| pending.phase != Phase::Running)
        {
            VerifiedInputs::default()
        } else {
            match self.gather_inputs(spec, body, source) {
                Ok(inputs) => inputs,
                Err(refusal) => return Ok(Attempt::Refused(refusal)),
            }
        };
        let persists = matches!(source, InputSource::Inbox);
        let mut pending = match resumed {
            Some(mut pending) => {
                if pending.phase == Phase::Running {
                    pending.attempts += 1;
                }
                pending
            }
            None => Pending {
                request_digest: candidate.digest.clone(),
                inbox_name: candidate.name.clone(),
                request: candidate.request.clone(),
                attempts: 1,
                phase: Phase::Running,
                measured_preconditions: measured.clone(),
                result: None,
                discarded_grant_digests: Vec::new(),
                started_at: lillux::time::iso8601_now(),
            },
        };
        if persists {
            self.store.save_pending(&pending)?;
        }
        let context = HandlerContext {
            app_root: self.app_root,
            state_lock: self.lock,
            request: body,
            request_digest: &candidate.digest,
            inputs: &inputs,
            host: self.host,
            node: self.node,
        };
        let outcome = self.run_handler(spec, &context, state, &mut pending, persists);
        let (receipt, _) = match outcome {
            Ok(done) => done,
            Err(error) => {
                if persists && pending.attempts >= MAX_STARTUP_ATTEMPTS {
                    return Ok(Attempt::Refused(Refusal::new(
                        RefusalReason::Stuck,
                        format!(
                            "the handler failed {} startups running: {error:#}",
                            pending.attempts
                        ),
                    )));
                }
                return Ok(Attempt::Failed(error));
            }
        };
        if persists {
            self.finish(candidate)?;
        }
        Ok(Attempt::Applied {
            sequence: receipt.body.sequence,
            operation: spec.id,
            authority_bearing: spec.authority_bearing,
        })
    }

    /// Measure every fact the request pins and compare.
    fn check_preconditions(
        &self,
        spec: &OperationSpec,
        body: &RequestBody,
        ledger_head: &str,
    ) -> std::result::Result<BTreeMap<String, String>, Refusal> {
        let facts = measure(self.app_root, ledger_head);
        let mut measured = BTreeMap::new();
        for fact in MANDATORY_FACTS.iter().chain(spec.preconditions) {
            let pinned = body.precondition.get(*fact).map(String::as_str);
            let actual = facts.get(*fact).map(String::as_str);
            if pinned != actual || actual.is_none() {
                return Err(Refusal::new(
                    RefusalReason::PreconditionFailed,
                    format!(
                        "fact `{fact}` is {} on this node, not the {} the request was signed against",
                        actual.unwrap_or("unmeasured"),
                        pinned.unwrap_or("unpinned")
                    ),
                ));
            }
            measured.insert((*fact).to_owned(), actual.unwrap_or_default().to_owned());
        }
        Ok(measured)
    }

    /// Open each input once, check its size against the slot, hash the
    /// bytes read from that same open descriptor, and hand the handler those
    /// verified bytes. Nothing is read again by path.
    fn gather_inputs(
        &self,
        spec: &OperationSpec,
        body: &RequestBody,
        source: &InputSource<'_>,
    ) -> std::result::Result<VerifiedInputs, Refusal> {
        let mut slots = BTreeMap::new();
        for (slot, digest) in &body.inputs {
            let Some(definition) = spec.slot(slot) else {
                return Err(Refusal::new(
                    RefusalReason::InvalidInputs,
                    format!("`{}` has no input slot `{slot}`", spec.id),
                ));
            };
            let read = match source {
                InputSource::Inbox => self
                    .store
                    .read_input(digest, definition.max_bytes)
                    .map_err(|e| Refusal::new(RefusalReason::InputMissing, format!("{e:#}")))?,
                InputSource::Memory(inputs) => match inputs.get(digest) {
                    None => Read::Missing,
                    Some(bytes)
                        if u64::try_from(bytes.len()).unwrap_or(u64::MAX)
                            > definition.max_bytes =>
                    {
                        Read::TooLarge
                    }
                    Some(bytes) => Read::Bytes(bytes.clone()),
                },
            };
            match read {
                Read::Missing => {
                    return Err(Refusal::new(
                        RefusalReason::InputMissing,
                        format!("input `{slot}` ({digest}) is not in the inbox"),
                    ));
                }
                Read::TooLarge => {
                    return Err(Refusal::new(
                        RefusalReason::InputOversized,
                        format!(
                            "input `{slot}` exceeds its {} byte bound",
                            definition.max_bytes
                        ),
                    ));
                }
                Read::Bytes(bytes) => {
                    if lillux::sha256_hex(&bytes) != *digest {
                        return Err(Refusal::new(
                            RefusalReason::InputMismatch,
                            format!(
                                "input `{slot}` does not hash to the digest the request pinned"
                            ),
                        ));
                    }
                    slots.insert(slot.clone(), bytes);
                }
            }
        }
        Ok(VerifiedInputs::new(slots))
    }

    /// Steps 6 to 8: the handler, the check of what it reported, and the
    /// receipt. The order of effect and receipt follows the handler's shape.
    fn run_handler(
        &self,
        spec: &OperationSpec,
        context: &HandlerContext<'_>,
        state: &LedgerState,
        pending: &mut Pending,
        persists: bool,
    ) -> Result<(SignedReceipt, Value)> {
        let request = &pending.request;
        match spec.handler {
            Handler::EffectThenReceipt(effect) => {
                let result = if pending.phase == Phase::Committed {
                    pending
                        .result
                        .clone()
                        .context("an intent record in the committed phase carries no result")?
                } else {
                    let result = effect(context)?;
                    verify_expected(&context.request.expected_result, &result)
                        .map_err(|refusal| anyhow!("{refusal}"))?;
                    pending.phase = Phase::Committed;
                    pending.result = Some(result.clone());
                    if persists {
                        self.store.save_pending(pending)?;
                    }
                    result
                };
                let (receipt, _) = self.ledger().append(
                    state,
                    ReceiptDraft {
                        request: request.clone(),
                        measured_preconditions: pending.measured_preconditions.clone(),
                        result: result.clone(),
                        discarded_grant_digests: Vec::new(),
                    },
                )?;
                Ok((receipt, result))
            }
            Handler::ReceiptThenEffect { prepare, effect } => {
                let receipt = if pending.phase == Phase::Receipted {
                    // The receipt is already in the ledger; find it.
                    self.receipt_of(&pending.request_digest)?
                } else {
                    let prepared = prepare(context)?;
                    pending.discarded_grant_digests = prepared.discarded_grant_digests.clone();
                    let (receipt, _) = self.ledger().append(
                        state,
                        ReceiptDraft {
                            request: request.clone(),
                            measured_preconditions: pending.measured_preconditions.clone(),
                            result: json!({ "effect": "follows this receipt" }),
                            discarded_grant_digests: prepared.discarded_grant_digests,
                        },
                    )?;
                    pending.phase = Phase::Receipted;
                    if persists {
                        self.store.save_pending(pending)?;
                    }
                    receipt
                };
                let result = effect(context)?;
                Ok((receipt, result))
            }
        }
    }

    fn receipt_of(&self, request_digest: &str) -> Result<SignedReceipt> {
        let ledger = self.ledger();
        let state = ledger.load()?;
        ledger
            .find_receipt(&state, request_digest)?
            .with_context(|| format!("no receipt for request {request_digest} in the ledger"))
    }
}

/// Step 7: every result fact the request pinned must be what the handler
/// reported.
pub(super) fn verify_expected(
    expected: &BTreeMap<String, String>,
    result: &Value,
) -> std::result::Result<(), Refusal> {
    for (fact, want) in expected {
        let got = result.get(fact).and_then(Value::as_str);
        if got != Some(want.as_str()) {
            return Err(Refusal::new(
                RefusalReason::ExpectedResultMismatch,
                format!(
                    "result fact `{fact}` is {}, not the {want} the request pinned",
                    got.unwrap_or("unreported")
                ),
            ));
        }
    }
    Ok(())
}

// ── The local operator's path ────────────────────────────────────────────

/// What the command line asks to have applied.
pub struct LocalDraft {
    pub operation: String,
    pub arguments: Value,
    /// Input bytes by slot.
    pub inputs: BTreeMap<String, Vec<u8>>,
    pub expected_result: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct LocalApplied {
    pub receipt: SignedReceipt,
    pub result: Value,
}

/// Sign a request with the on-volume local operator key and apply it in
/// process, under the node's state lock. The node must be stopped: a running
/// daemon holds the same lock.
pub fn apply_local(
    app_root: &Path,
    host: &dyn OfflineHost,
    draft: LocalDraft,
) -> Result<LocalApplied> {
    apply_local_with_options(app_root, host, draft, &ApplyOptions::default())
}

pub fn apply_local_with_options(
    app_root: &Path,
    host: &dyn OfflineHost,
    draft: LocalDraft,
    options: &ApplyOptions,
) -> Result<LocalApplied> {
    validate_table().context("the compiled operation table is invalid")?;
    let spec = lookup(&draft.operation)
        .with_context(|| format!("this build has no offline operation `{}`", draft.operation))?;
    // A running daemon holds the state lock, so the refusal comes before
    // anything else is read: the operation cannot race a live node.
    let lock = StateLock::acquire(&default_lock_path(app_root))
        .context("offline operations apply to a stopped node, and its state lock is held")?;
    lock.ensure_protects_app_root(app_root)?;
    let node = node_identity(app_root)
        .map_err(|error| anyhow!("{error}"))?
        .context("the node has no identity; it is not initialized")?;
    let store = Store::for_app_root(app_root);
    store.create_layout()?;
    let ledger = Ledger::new(&store, &node);
    let state = ledger
        .load()
        .context("verify the offline-operation ledger")?;
    let operator_path = RuntimeRoot::new(app_root.to_path_buf()).operator_signing_key_path();
    let operator = lillux::crypto::load_signing_key(&operator_path)
        .with_context(|| format!("load the local operator key {}", operator_path.display()))?;
    let facts = measure(app_root, &state.head);
    let mut precondition = BTreeMap::new();
    for fact in MANDATORY_FACTS.iter().chain(spec.preconditions) {
        precondition.insert(
            (*fact).to_owned(),
            facts.get(*fact).cloned().unwrap_or_default(),
        );
    }
    let now = (options.clock)();
    let digests: BTreeMap<String, String> = draft
        .inputs
        .iter()
        .map(|(slot, bytes)| (slot.clone(), lillux::sha256_hex(bytes)))
        .collect();
    let by_digest: BTreeMap<String, Vec<u8>> = draft
        .inputs
        .into_values()
        .map(|bytes| (lillux::sha256_hex(&bytes), bytes))
        .collect();
    let request = SignedRequest::sign(
        RequestBody {
            schema: REQUEST_SCHEMA.to_owned(),
            node_fingerprint: node.fingerprint().to_owned(),
            signer_fingerprint: lillux::signature::compute_fingerprint(&operator.verifying_key()),
            signer_grant_digest: None,
            signer_origin_site_id: None,
            operation: draft.operation,
            arguments: draft.arguments,
            inputs: digests,
            expected_result: draft.expected_result,
            precondition,
            issued_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            not_after: (now + DEFAULT_REQUEST_LIFETIME)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        },
        &operator,
    )?;
    let digest = request.digest()?;
    let applier = Applier {
        app_root,
        host,
        node: &node,
        store: &store,
        lock: &lock,
        clock: options.clock,
    };
    let candidate = Candidate {
        name: digest.clone(),
        bytes: request.to_bytes()?,
        digest,
        request,
    };
    match applier.evaluate(&state, &candidate) {
        Verdict::Ready => {}
        Verdict::Refuse(refusal) => return Err(anyhow!("{refusal}")),
        Verdict::Wait => anyhow::bail!("the ledger head moved while the request was built"),
    }
    match applier.attempt(
        &state,
        &candidate,
        &InputSource::Memory(&by_digest),
        None,
        0,
    )? {
        Attempt::Applied { .. } => {
            let state = ledger.load()?;
            let receipt = ledger
                .find_receipt(&state, &candidate.digest)?
                .context("the receipt just written is not in the ledger")?;
            let result = receipt.body.result.clone();
            Ok(LocalApplied { receipt, result })
        }
        Attempt::Refused(refusal) => Err(anyhow!("{refusal}")),
        Attempt::Failed(error) => Err(error),
        Attempt::Deferred => anyhow::bail!("the operation must be applied alone"),
    }
}

// ── Reading, without the lock ────────────────────────────────────────────

/// The facts the node would be measured by, for the read-only command. It
/// reads the ledger and the volume and writes nothing, so it works with the
/// daemon stopped, and while it runs.
pub fn read_facts(app_root: &Path) -> Result<NodeFacts> {
    let node = node_identity(app_root)
        .map_err(|error| anyhow!("{error}"))?
        .context("the node has no identity; it is not initialized")?;
    let store = Store::for_app_root(app_root);
    let head = match Ledger::new(&store, &node).load() {
        Ok(state) => state.head,
        Err(_) => "unreadable".to_owned(),
    };
    Ok(measure(app_root, &head))
}

/// The node's ledger chain tip, verified.
pub fn read_ledger(app_root: &Path) -> Result<LedgerState> {
    let node = node_identity(app_root)
        .map_err(|error| anyhow!("{error}"))?
        .context("the node has no identity; it is not initialized")?;
    Ledger::new(&Store::for_app_root(app_root), &node).load()
}
