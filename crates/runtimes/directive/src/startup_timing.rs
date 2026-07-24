use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static PROCESS_TIMINGS: OnceLock<DirectiveStageTimings> = OnceLock::new();
const NO_ACTIVE_PROVIDER_CALL: u64 = 0;
const AMBIGUOUS_ACTIVE_PROVIDER_CALL: u64 = u64::MAX;
const MAX_TIMING_ID_BYTES: usize = 256;
const MAX_PROVIDER_ID_BYTES: usize = 256;
const MAX_PROVIDER_CALL_TIMING_RECORDS: usize = 127;

tokio::task_local! {
    static CURRENT_PROVIDER_CALL_ID: Option<u64>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
struct StageOffsets {
    invocation_id: Option<String>,
    thread_id: Option<String>,
    envelope_parsed_us: Option<u64>,
    attach_process_started_us: Option<u64>,
    attach_process_done_us: Option<u64>,
    mark_running_started_us: Option<u64>,
    mark_running_done_us: Option<u64>,
    bootstrap_done_us: Option<u64>,
    provider_request_submitted_us: Option<u64>,
    provider_response_headers_us: Option<u64>,
    provider_http_version: Option<String>,
    first_provider_event_us: Option<u64>,
    first_non_whitespace_text_us: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct ProviderCallOffsets {
    call_id: u64,
    provider_id: String,
    turn: u32,
    attempt: u32,
    call_started_us: u64,
    request_submitted_us: Option<u64>,
    response_headers_us: Option<u64>,
    http_version: Option<String>,
    first_provider_event_us: Option<u64>,
    dns_lookup_first_started_us: Option<u64>,
    dns_lookup_last_done_us: Option<u64>,
    dns_lookup_count: u32,
    dns_lookup_completed_count: u32,
    dns_lookup_total_us: u64,
    dns_lookup_failures: u32,
    connection_establishment_first_started_us: Option<u64>,
    connection_establishment_last_done_us: Option<u64>,
    connection_establishment_count: u32,
    connection_establishment_completed_count: u32,
    connection_establishment_total_us: u64,
    connection_establishment_failures: u32,
    call_finished_us: Option<u64>,
    completion: Option<&'static str>,
}

impl ProviderCallOffsets {
    fn mark_request_submitted(&mut self, elapsed_us: u64) {
        self.request_submitted_us.get_or_insert(elapsed_us);
    }

    fn mark_response_headers(&mut self, elapsed_us: u64, http_version: &str) {
        self.response_headers_us.get_or_insert(elapsed_us);
        self.http_version
            .get_or_insert_with(|| http_version.to_owned());
    }

    fn mark_first_provider_event(&mut self, elapsed_us: u64) {
        self.first_provider_event_us.get_or_insert(elapsed_us);
    }

    fn begin_dns_lookup(&mut self, started_us: u64) {
        self.dns_lookup_count = self.dns_lookup_count.saturating_add(1);
        self.dns_lookup_first_started_us = Some(
            self.dns_lookup_first_started_us
                .map_or(started_us, |previous| previous.min(started_us)),
        );
    }

    fn finish_dns_lookup(&mut self, started_us: u64, done_us: u64, succeeded: bool) {
        self.dns_lookup_last_done_us = Some(
            self.dns_lookup_last_done_us
                .map_or(done_us, |previous| previous.max(done_us)),
        );
        self.dns_lookup_completed_count = self.dns_lookup_completed_count.saturating_add(1);
        self.dns_lookup_total_us = self
            .dns_lookup_total_us
            .saturating_add(done_us.saturating_sub(started_us));
        if !succeeded {
            self.dns_lookup_failures = self.dns_lookup_failures.saturating_add(1);
        }
    }

    fn begin_connection_establishment(&mut self, started_us: u64) {
        self.connection_establishment_count = self.connection_establishment_count.saturating_add(1);
        self.connection_establishment_first_started_us = Some(
            self.connection_establishment_first_started_us
                .map_or(started_us, |previous| previous.min(started_us)),
        );
    }

    fn finish_connection_establishment(&mut self, started_us: u64, done_us: u64, succeeded: bool) {
        self.connection_establishment_last_done_us = Some(
            self.connection_establishment_last_done_us
                .map_or(done_us, |previous| previous.max(done_us)),
        );
        self.connection_establishment_completed_count = self
            .connection_establishment_completed_count
            .saturating_add(1);
        self.connection_establishment_total_us = self
            .connection_establishment_total_us
            .saturating_add(done_us.saturating_sub(started_us));
        if !succeeded {
            self.connection_establishment_failures =
                self.connection_establishment_failures.saturating_add(1);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TransportTimingToken {
    call_id: u64,
    started_us: u64,
}

/// Process-local monotonic timing for the directive runtime and every provider
/// call attempt it makes.
///
/// A directive runtime process executes exactly one launch envelope, so this
/// tracker deliberately has process scope. All values are offsets from entry
/// into `main`; they must never be subtracted from daemon-side timestamps.
pub struct DirectiveStageTimings {
    main_started_at: Instant,
    offsets: Mutex<StageOffsets>,
    provider_calls: Mutex<Vec<ProviderCallOffsets>>,
    next_provider_call_id: AtomicU64,
    active_provider_call_id: AtomicU64,
    first_provider_event_marked: AtomicBool,
    last_provider_call_with_first_event: AtomicU64,
    provider_call_limit_warned: AtomicBool,
    summary_emitted: AtomicBool,
}

impl DirectiveStageTimings {
    fn new(main_started_at: Instant) -> Self {
        Self {
            main_started_at,
            offsets: Mutex::new(StageOffsets::default()),
            provider_calls: Mutex::new(Vec::new()),
            next_provider_call_id: AtomicU64::new(1),
            active_provider_call_id: AtomicU64::new(NO_ACTIVE_PROVIDER_CALL),
            first_provider_event_marked: AtomicBool::new(false),
            last_provider_call_with_first_event: AtomicU64::new(NO_ACTIVE_PROVIDER_CALL),
            provider_call_limit_warned: AtomicBool::new(false),
            summary_emitted: AtomicBool::new(false),
        }
    }

    fn elapsed_us(&self) -> u64 {
        u64::try_from(self.main_started_at.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    fn with_offsets(&self, update: impl FnOnce(&mut StageOffsets, u64)) {
        let elapsed_us = self.elapsed_us();
        let mut offsets = self
            .offsets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        update(&mut offsets, elapsed_us);
    }

    fn set_identity(&self, invocation_id: &str, thread_id: &str) {
        self.with_offsets(|offsets, _| {
            if bounded_nonempty(invocation_id, MAX_TIMING_ID_BYTES) {
                offsets
                    .invocation_id
                    .get_or_insert_with(|| invocation_id.to_owned());
            }
            if bounded_nonempty(thread_id, MAX_TIMING_ID_BYTES) {
                offsets
                    .thread_id
                    .get_or_insert_with(|| thread_id.to_owned());
            }
        });
    }

    fn mark(&self, select: impl FnOnce(&mut StageOffsets) -> &mut Option<u64>) {
        self.with_offsets(|offsets, elapsed_us| {
            select(offsets).get_or_insert(elapsed_us);
        });
    }

    fn snapshot(&self) -> StageOffsets {
        self.offsets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn begin_provider_call(&self, provider_id: &str, turn: u32, attempt: u32) -> Option<u64> {
        let mut calls = self
            .provider_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if calls.len() >= MAX_PROVIDER_CALL_TIMING_RECORDS {
            if !self.provider_call_limit_warned.swap(true, Ordering::AcqRel) {
                tracing::warn!(
                    target: "ryeos_directive_runtime",
                    provider_call_timing_record_limit = MAX_PROVIDER_CALL_TIMING_RECORDS,
                    "provider call timing record limit reached; later calls run uninstrumented"
                );
            }
            return None;
        }
        let call_id = self.next_provider_call_id.fetch_add(1, Ordering::Relaxed);
        let call = ProviderCallOffsets {
            call_id,
            provider_id: if bounded_nonempty(provider_id, MAX_PROVIDER_ID_BYTES) {
                provider_id.to_owned()
            } else {
                "<invalid-provider-id>".to_string()
            },
            turn,
            attempt,
            call_started_us: self.elapsed_us(),
            request_submitted_us: None,
            response_headers_us: None,
            http_version: None,
            first_provider_event_us: None,
            dns_lookup_first_started_us: None,
            dns_lookup_last_done_us: None,
            dns_lookup_count: 0,
            dns_lookup_completed_count: 0,
            dns_lookup_total_us: 0,
            dns_lookup_failures: 0,
            connection_establishment_first_started_us: None,
            connection_establishment_last_done_us: None,
            connection_establishment_count: 0,
            connection_establishment_completed_count: 0,
            connection_establishment_total_us: 0,
            connection_establishment_failures: 0,
            call_finished_us: None,
            completion: None,
        };
        calls.push(call);
        drop(calls);
        if self
            .active_provider_call_id
            .compare_exchange(
                NO_ACTIVE_PROVIDER_CALL,
                call_id,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            // Provider calls are sequential in the directive runner. If that
            // invariant ever changes, spawned reqwest tasks without a Tokio
            // task-local cannot be attributed safely, so fail closed.
            self.active_provider_call_id
                .store(AMBIGUOUS_ACTIVE_PROVIDER_CALL, Ordering::Release);
        }
        Some(call_id)
    }

    fn with_provider_call(&self, call_id: u64, update: impl FnOnce(&mut ProviderCallOffsets, u64)) {
        let elapsed_us = self.elapsed_us();
        let mut calls = self
            .provider_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(call) = calls.iter_mut().find(|call| call.call_id == call_id) {
            update(call, elapsed_us);
        }
    }

    fn first_provider_call_snapshot(&self) -> Option<ProviderCallOffsets> {
        self.provider_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .first()
            .cloned()
    }

    fn active_provider_call_fallback(&self) -> Option<u64> {
        let call_id = self.active_provider_call_id.load(Ordering::Acquire);
        (call_id != NO_ACTIVE_PROVIDER_CALL && call_id != AMBIGUOUS_ACTIVE_PROVIDER_CALL)
            .then_some(call_id)
    }

    fn mark_first_provider_event(&self, call_id: Option<u64>) {
        let process_already_marked = self
            .first_provider_event_marked
            .swap(true, Ordering::AcqRel);
        let call_already_marked = call_id.is_none_or(|call_id| {
            self.last_provider_call_with_first_event
                .load(Ordering::Acquire)
                == call_id
        });
        if process_already_marked && call_already_marked {
            return;
        }
        if !process_already_marked {
            self.mark(|offsets| &mut offsets.first_provider_event_us);
        }
        if let Some(call_id) = call_id.filter(|_| !call_already_marked) {
            self.with_provider_call(call_id, |call, elapsed_us| {
                call.mark_first_provider_event(elapsed_us);
            });
            self.last_provider_call_with_first_event
                .store(call_id, Ordering::Release);
        }
    }

    fn finish_provider_call(&self, call_id: u64, completion: &'static str) {
        let elapsed_us = self.elapsed_us();
        let call = {
            let mut calls = self
                .provider_calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(call) = calls.iter_mut().find(|call| call.call_id == call_id) else {
                return;
            };
            call.call_finished_us.get_or_insert(elapsed_us);
            call.completion.get_or_insert(completion);
            call.clone()
        };
        self.emit_provider_call_summary(&call);
        let _ = self.active_provider_call_id.compare_exchange(
            call_id,
            NO_ACTIVE_PROVIDER_CALL,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn emit_provider_call_summary(&self, call: &ProviderCallOffsets) {
        let identity = self.snapshot();
        let connection_establishment_observed = call.connection_establishment_count > 0;

        tracing::info!(
            target: "ryeos_directive_runtime",
            event = "directive_provider_call_timing",
            schema_version = 1_u32,
            invocation_id = identity.invocation_id.as_deref().unwrap_or("<unavailable>"),
            thread_id = identity.thread_id.as_deref().unwrap_or("<unavailable>"),
            item_ref_kind = "directive",
            clock_domain = "directive_process_monotonic",
            provider_call_id = call.call_id,
            provider_id = call.provider_id.as_str(),
            turn = call.turn,
            attempt = call.attempt,
            completion = call.completion,
            call_started_us = call.call_started_us,
            request_submitted_us = call.request_submitted_us,
            response_headers_us = call.response_headers_us,
            request_to_headers_us = duration_us(
                call.request_submitted_us,
                call.response_headers_us,
            ),
            provider_http_version = call.http_version.as_deref(),
            first_provider_event_us = call.first_provider_event_us,
            headers_to_first_event_us = duration_us(
                call.response_headers_us,
                call.first_provider_event_us,
            ),
            request_to_first_event_us = duration_us(
                call.request_submitted_us,
                call.first_provider_event_us,
            ),
            dns_lookup_first_started_us = call.dns_lookup_first_started_us,
            dns_lookup_last_done_us = call.dns_lookup_last_done_us,
            dns_lookup_count = call.dns_lookup_count,
            dns_lookup_completed_count = call.dns_lookup_completed_count,
            dns_lookup_total_us = call.dns_lookup_total_us,
            dns_lookup_failures = call.dns_lookup_failures,
            dns_lookup_scope = "exact_resolver_future",
            connection_establishment_observed,
            connection_establishment_first_started_us =
                call.connection_establishment_first_started_us,
            connection_establishment_last_done_us =
                call.connection_establishment_last_done_us,
            connection_establishment_count = call.connection_establishment_count,
            connection_establishment_completed_count =
                call.connection_establishment_completed_count,
            connection_establishment_total_us = call.connection_establishment_total_us,
            connection_establishment_failures = call.connection_establishment_failures,
            connection_establishment_scope =
                "aggregate_reqwest_connector_may_include_dns_tcp_proxy_tls",
            exact_tcp_tls_split_available = false,
            call_finished_us = call.call_finished_us,
            call_duration_us = duration_us(Some(call.call_started_us), call.call_finished_us),
            "directive provider call timings"
        );
        emit_captured_timing_record(serde_json::json!({
            "event": "directive_provider_call_timing",
            "schema_version": 1,
            "clock_domain": "directive_process_monotonic",
            "invocation_id": identity.invocation_id,
            "thread_id": identity.thread_id,
            "item_ref_kind": "directive",
            "dns_lookup_scope": "exact_resolver_future",
            "connection_establishment_scope":
                "aggregate_reqwest_connector_may_include_dns_tcp_proxy_tls",
            "exact_tcp_tls_split_available": false,
            "timing": call,
        }));
    }

    fn emit_summary_once(&self, completion: &'static str) {
        if self.summary_emitted.swap(true, Ordering::AcqRel) {
            return;
        }

        let offsets = self.snapshot();
        let first_provider_call = self.first_provider_call_snapshot();
        let attach_process_duration_us = duration_us(
            offsets.attach_process_started_us,
            offsets.attach_process_done_us,
        );
        let mark_running_duration_us = duration_us(
            offsets.mark_running_started_us,
            offsets.mark_running_done_us,
        );

        tracing::info!(
            target: "ryeos_directive_runtime",
            event = "directive_runtime_stage_timing",
            schema_version = 2_u32,
            invocation_id = offsets.invocation_id.as_deref().unwrap_or("<unavailable>"),
            thread_id = offsets.thread_id.as_deref().unwrap_or("<unavailable>"),
            item_ref_kind = "directive",
            clock_domain = "directive_process_monotonic",
            completion,
            main_started_us = 0_u64,
            envelope_parsed_us = offsets.envelope_parsed_us,
            attach_process_started_us = offsets.attach_process_started_us,
            attach_process_done_us = offsets.attach_process_done_us,
            attach_process_duration_us,
            mark_running_started_us = offsets.mark_running_started_us,
            mark_running_done_us = offsets.mark_running_done_us,
            mark_running_duration_us,
            bootstrap_done_us = offsets.bootstrap_done_us,
            provider_request_submitted_us = offsets.provider_request_submitted_us,
            provider_response_headers_us = offsets.provider_response_headers_us,
            provider_http_version = offsets.provider_http_version.as_deref(),
            first_provider_event_us = offsets.first_provider_event_us,
            first_non_whitespace_text_us = offsets.first_non_whitespace_text_us,
            provider_dns_lookup_first_started_us = first_provider_call
                .as_ref()
                .and_then(|call| call.dns_lookup_first_started_us),
            provider_dns_lookup_last_done_us = first_provider_call
                .as_ref()
                .and_then(|call| call.dns_lookup_last_done_us),
            provider_dns_lookup_total_us =
                first_provider_call.as_ref().map(|call| call.dns_lookup_total_us),
            provider_dns_lookup_count =
                first_provider_call.as_ref().map(|call| call.dns_lookup_count),
            provider_dns_lookup_completed_count = first_provider_call
                .as_ref()
                .map(|call| call.dns_lookup_completed_count),
            provider_connection_establishment_first_started_us = first_provider_call
                .as_ref()
                .and_then(|call| call.connection_establishment_first_started_us),
            provider_connection_establishment_last_done_us = first_provider_call
                .as_ref()
                .and_then(|call| call.connection_establishment_last_done_us),
            provider_connection_establishment_total_us = first_provider_call
                .as_ref()
                .map(|call| call.connection_establishment_total_us),
            provider_connection_establishment_count = first_provider_call
                .as_ref()
                .map(|call| call.connection_establishment_count),
            provider_connection_establishment_completed_count = first_provider_call
                .as_ref()
                .map(|call| call.connection_establishment_completed_count),
            provider_connection_establishment_scope =
                "aggregate_reqwest_connector_may_include_dns_tcp_proxy_tls",
            exact_tcp_tls_split_available = false,
            summary_emitted_us = self.elapsed_us(),
            "directive runtime stage timings"
        );
        emit_captured_timing_record(serde_json::json!({
            "event": "directive_runtime_stage_timing",
            "schema_version": 2,
            "clock_domain": "directive_process_monotonic",
            "invocation_id": offsets.invocation_id,
            "thread_id": offsets.thread_id,
            "item_ref_kind": "directive",
            "connection_establishment_scope":
                "aggregate_reqwest_connector_may_include_dns_tcp_proxy_tls",
            "exact_tcp_tls_split_available": false,
            "timing": offsets,
            "first_provider_call": first_provider_call,
            "summary_emitted_us": self.elapsed_us(),
            "completion": completion,
        }));
    }
}

fn emit_captured_timing_record(record: serde_json::Value) {
    match serde_json::to_string(&record) {
        Ok(encoded) => eprintln!(
            "{}{}",
            ryeos_runtime::events::CAPTURED_CHILD_TIMING_PREFIX,
            encoded
        ),
        Err(error) => tracing::warn!(
            target: "ryeos_directive_runtime",
            %error,
            "failed to encode captured child timing record"
        ),
    }
}

fn bounded_nonempty(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes
}

fn duration_us(started_us: Option<u64>, done_us: Option<u64>) -> Option<u64> {
    Some(done_us?.saturating_sub(started_us?))
}

pub fn init(main_started_at: Instant) -> &'static DirectiveStageTimings {
    PROCESS_TIMINGS.get_or_init(|| DirectiveStageTimings::new(main_started_at))
}

fn process_timings() -> Option<&'static DirectiveStageTimings> {
    PROCESS_TIMINGS.get()
}

fn current_provider_call_id() -> Option<u64> {
    if let Ok(call_id) = CURRENT_PROVIDER_CALL_ID.try_with(|call_id| *call_id) {
        // A present task-local containing `None` deliberately disables
        // attribution after the bounded record limit. Do not fall through and
        // misattribute that call to an unrelated process-wide active call.
        return call_id;
    }

    process_timings()?.active_provider_call_fallback()
}

pub fn begin_provider_call(provider_id: &str, turn: u32, attempt: u32) -> Option<u64> {
    process_timings()?.begin_provider_call(provider_id, turn, attempt)
}

pub async fn scope_provider_call<F>(call_id: Option<u64>, future: F) -> F::Output
where
    F: Future,
{
    CURRENT_PROVIDER_CALL_ID.scope(call_id, future).await
}

pub fn finish_provider_call(call_id: Option<u64>, completion: &'static str) {
    if let (Some(timings), Some(call_id)) = (process_timings(), call_id) {
        timings.finish_provider_call(call_id, completion);
    }
}

pub(crate) fn begin_dns_lookup() -> Option<TransportTimingToken> {
    let timings = process_timings()?;
    let call_id = current_provider_call_id()?;
    let started_us = timings.elapsed_us();
    timings.with_provider_call(call_id, |call, _| {
        call.begin_dns_lookup(started_us);
    });
    Some(TransportTimingToken {
        call_id,
        started_us,
    })
}

pub(crate) fn finish_dns_lookup(token: Option<TransportTimingToken>, succeeded: bool) {
    let (Some(timings), Some(token)) = (process_timings(), token) else {
        return;
    };
    timings.with_provider_call(token.call_id, |call, done_us| {
        call.finish_dns_lookup(token.started_us, done_us, succeeded);
    });
}

pub(crate) fn begin_connection_establishment() -> Option<TransportTimingToken> {
    let timings = process_timings()?;
    let call_id = current_provider_call_id()?;
    let started_us = timings.elapsed_us();
    timings.with_provider_call(call_id, |call, _| {
        call.begin_connection_establishment(started_us);
    });
    Some(TransportTimingToken {
        call_id,
        started_us,
    })
}

pub(crate) fn finish_connection_establishment(
    token: Option<TransportTimingToken>,
    succeeded: bool,
) {
    let (Some(timings), Some(token)) = (process_timings(), token) else {
        return;
    };
    timings.with_provider_call(token.call_id, |call, done_us| {
        call.finish_connection_establishment(token.started_us, done_us, succeeded);
    });
}

pub fn set_identity(invocation_id: &str, thread_id: &str) {
    if let Some(timings) = process_timings() {
        timings.set_identity(invocation_id, thread_id);
    }
}

macro_rules! stage_marker {
    ($name:ident, $field:ident) => {
        pub fn $name() {
            if let Some(timings) = process_timings() {
                timings.mark(|offsets| &mut offsets.$field);
            }
        }
    };
}

stage_marker!(mark_envelope_parsed, envelope_parsed_us);
stage_marker!(mark_attach_process_started, attach_process_started_us);
stage_marker!(mark_attach_process_done, attach_process_done_us);
stage_marker!(mark_mark_running_started, mark_running_started_us);
stage_marker!(mark_mark_running_done, mark_running_done_us);
stage_marker!(mark_bootstrap_done, bootstrap_done_us);
pub fn mark_provider_request_submitted() {
    if let Some(timings) = process_timings() {
        timings.mark(|offsets| &mut offsets.provider_request_submitted_us);
        if let Some(call_id) = current_provider_call_id() {
            timings.with_provider_call(call_id, |call, elapsed_us| {
                call.mark_request_submitted(elapsed_us);
            });
        }
    }
}

pub fn mark_first_provider_event() {
    if let Some(timings) = process_timings() {
        timings.mark_first_provider_event(current_provider_call_id());
    }
}

pub fn mark_provider_response_headers(http_version: &str) {
    if let Some(timings) = process_timings() {
        timings.with_offsets(|offsets, elapsed_us| {
            offsets
                .provider_response_headers_us
                .get_or_insert(elapsed_us);
            offsets
                .provider_http_version
                .get_or_insert_with(|| http_version.to_owned());
        });
        if let Some(call_id) = current_provider_call_id() {
            timings.with_provider_call(call_id, |call, elapsed_us| {
                call.mark_response_headers(elapsed_us, http_version);
            });
        }
    }
}

/// Record the closest child-side proxy for downstream `first_text`: the daemon
/// has acknowledged publication of a delta containing visible text. Actual
/// downstream delivery remains outside this process's clock domain.
pub fn mark_first_non_whitespace_text_published(text: &str) {
    if text.trim().is_empty() {
        return;
    }
    if let Some(timings) = process_timings() {
        if timings.summary_emitted.load(Ordering::Acquire) {
            return;
        }
        timings.mark(|offsets| &mut offsets.first_non_whitespace_text_us);
        timings.emit_summary_once("first_non_whitespace_text_published");
    }
}

pub fn emit_process_exit_summary() {
    if let Some(timings) = process_timings() {
        timings.emit_summary_once("process_exit");
    }
}

#[cfg(test)]
mod tests {
    use super::{duration_us, DirectiveStageTimings, MAX_PROVIDER_CALL_TIMING_RECORDS};
    use std::time::Instant;

    #[test]
    fn stage_offsets_are_first_write_wins() {
        let timings = DirectiveStageTimings::new(Instant::now());

        timings.mark(|offsets| &mut offsets.envelope_parsed_us);
        let first = timings.snapshot().envelope_parsed_us.expect("first marker");
        timings.mark(|offsets| &mut offsets.envelope_parsed_us);

        assert_eq!(timings.snapshot().envelope_parsed_us, Some(first));
    }

    #[test]
    fn rpc_duration_uses_offsets_in_the_same_clock_domain() {
        assert_eq!(duration_us(Some(7), Some(19)), Some(12));
        assert_eq!(duration_us(None, Some(19)), None);
        assert_eq!(duration_us(Some(19), None), None);
    }

    #[test]
    fn identity_is_first_write_wins() {
        let timings = DirectiveStageTimings::new(Instant::now());

        timings.set_identity("invocation-1", "thread-1");
        timings.set_identity("invocation-2", "thread-2");

        let snapshot = timings.snapshot();
        assert_eq!(snapshot.invocation_id.as_deref(), Some("invocation-1"));
        assert_eq!(snapshot.thread_id.as_deref(), Some("thread-1"));
    }

    #[test]
    fn provider_call_stage_offsets_are_first_write_wins() {
        let timings = DirectiveStageTimings::new(Instant::now());
        let call_id = timings
            .begin_provider_call("provider-a", 3, 1)
            .expect("provider call timing slot");

        timings.with_provider_call(call_id, |call, _| {
            call.mark_request_submitted(11);
            call.mark_response_headers(23, "HTTP/2.0");
            call.mark_first_provider_event(29);
            call.mark_request_submitted(101);
            call.mark_response_headers(103, "HTTP/1.1");
            call.mark_first_provider_event(109);
        });

        let call = timings
            .first_provider_call_snapshot()
            .expect("provider call");
        assert_eq!(call.call_id, call_id);
        assert_eq!(call.provider_id, "provider-a");
        assert_eq!(call.turn, 3);
        assert_eq!(call.attempt, 1);
        assert_eq!(call.request_submitted_us, Some(11));
        assert_eq!(call.response_headers_us, Some(23));
        assert_eq!(call.http_version.as_deref(), Some("HTTP/2.0"));
        assert_eq!(call.first_provider_event_us, Some(29));
    }

    #[test]
    fn transport_aggregation_counts_failures_and_uses_outer_bounds() {
        let timings = DirectiveStageTimings::new(Instant::now());
        let call_id = timings
            .begin_provider_call("provider-a", 3, 1)
            .expect("provider call timing slot");

        timings.with_provider_call(call_id, |call, _| {
            call.begin_dns_lookup(20);
            call.begin_dns_lookup(10);
            call.finish_dns_lookup(20, 50, true);
            call.finish_dns_lookup(10, 45, false);

            call.begin_connection_establishment(100);
            call.begin_connection_establishment(90);
            call.finish_connection_establishment(100, 160, true);
            call.finish_connection_establishment(90, 120, false);
        });

        let call = timings
            .first_provider_call_snapshot()
            .expect("provider call");
        assert_eq!(call.dns_lookup_first_started_us, Some(10));
        assert_eq!(call.dns_lookup_last_done_us, Some(50));
        assert_eq!(call.dns_lookup_count, 2);
        assert_eq!(call.dns_lookup_completed_count, 2);
        assert_eq!(call.dns_lookup_total_us, 65);
        assert_eq!(call.dns_lookup_failures, 1);
        assert_eq!(call.connection_establishment_first_started_us, Some(90));
        assert_eq!(call.connection_establishment_last_done_us, Some(160));
        assert_eq!(call.connection_establishment_count, 2);
        assert_eq!(call.connection_establishment_completed_count, 2);
        assert_eq!(call.connection_establishment_total_us, 90);
        assert_eq!(call.connection_establishment_failures, 1);
    }

    #[test]
    fn overlapping_calls_disable_spawned_task_fallback_attribution() {
        let sequential = DirectiveStageTimings::new(Instant::now());
        let sequential_call = sequential
            .begin_provider_call("provider-a", 2, 1)
            .expect("provider call timing slot");
        assert_eq!(
            sequential.active_provider_call_fallback(),
            Some(sequential_call)
        );
        sequential.finish_provider_call(sequential_call, "completed");
        assert_eq!(sequential.active_provider_call_fallback(), None);

        let timings = DirectiveStageTimings::new(Instant::now());
        let first = timings
            .begin_provider_call("provider-a", 3, 1)
            .expect("provider call timing slot");
        assert_eq!(timings.active_provider_call_fallback(), Some(first));

        let second = timings
            .begin_provider_call("provider-a", 3, 2)
            .expect("provider call timing slot");
        assert_eq!(timings.active_provider_call_fallback(), None);

        timings.finish_provider_call(first, "completed");
        assert_eq!(timings.active_provider_call_fallback(), None);
        timings.finish_provider_call(second, "completed");
        assert_eq!(timings.active_provider_call_fallback(), None);
    }

    #[test]
    fn provider_call_timing_records_are_bounded() {
        let timings = DirectiveStageTimings::new(Instant::now());
        for attempt in 0..MAX_PROVIDER_CALL_TIMING_RECORDS {
            assert!(
                timings
                    .begin_provider_call("provider-a", 1, (attempt + 1) as u32)
                    .is_some(),
                "timing slot within the bound"
            );
        }

        assert!(
            timings
                .begin_provider_call(
                    "provider-a",
                    1,
                    (MAX_PROVIDER_CALL_TIMING_RECORDS + 1) as u32,
                )
                .is_none(),
            "calls beyond the observability bound must run without a timing record"
        );
    }
}
