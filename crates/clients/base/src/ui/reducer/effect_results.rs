use super::dto::{
    RyeOsAddProjectDto, RyeOsDimensionDto, RyeOsFileReadDto, RyeOsFileSpaceDto, RyeOsFilesDto,
    RyeOsItemsDto, RyeOsOpenProjectDto, RyeOsThreadsDto, RyeOsTopologyDto,
};
use super::effect::{RyeOsEffect, RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind};
use super::model::RyeOsCore;
use super::parse_tile_id;
use super::view_model::RyeOsTone;

const INPUT_QUEUE_NOTICE_PREFIX: &str = "Queued behind active thread";

impl RyeOsCore {
    pub(crate) fn apply_effect_result(&mut self, result: RyeOsEffectResult) -> Vec<RyeOsEffect> {
        let Some(expected) = self.pending_effects.remove(&result.id) else {
            return Vec::new();
        };
        let completed_source_key = match &expected {
            RyeOsEffectKind::FetchSource { tile_id, .. } => Some(tile_id.clone()),
            _ => None,
        };

        if !effect_result_kind_matches(&expected, &result.kind) {
            self.notice(
                "RyeOS ignored a mismatched platform effect result.",
                RyeOsTone::Warn,
            );
            return self.finish_source_effect(completed_source_key.as_deref(), Vec::new());
        }

        if !result.ok {
            if let Some(source_key) = completed_source_key.as_deref() {
                self.record_source_failure(source_key, result.id, result.error.as_deref());
            }
            self.notice(
                effect_failure_notice(&expected, result.error.as_deref()),
                RyeOsTone::Danger,
            );
            return self.finish_source_effect(completed_source_key.as_deref(), Vec::new());
        }

        if matches!(
            result.kind,
            RyeOsEffectResultKind::InvocationDispatch
                | RyeOsEffectResultKind::ThreadCommandSubmitted
                | RyeOsEffectResultKind::Invoked
        ) {
            let data = result
                .data
                .as_ref()
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let effects = self.apply_invocation_result(&expected, result.kind, data);
            return self.finish_source_effect(completed_source_key.as_deref(), effects);
        }

        let Some(data) = result.data else {
            self.bump_generation();
            return self.finish_source_effect(completed_source_key.as_deref(), Vec::new());
        };

        let effects = self.apply_source_result(&expected, result.kind, result.id, data);
        self.finish_source_effect(completed_source_key.as_deref(), effects)
    }

    fn finish_source_effect(
        &mut self,
        completed_source_key: Option<&str>,
        mut effects: Vec<RyeOsEffect>,
    ) -> Vec<RyeOsEffect> {
        if let Some(effect) = completed_source_key
            .and_then(|source_key| self.release_deferred_source_fetch(source_key))
        {
            effects.push(effect);
        }
        effects
    }

    fn record_source_failure(&mut self, source_key: &str, result_id: u64, error: Option<&str>) {
        let floor = self
            .data
            .source_floor
            .get(source_key)
            .copied()
            .unwrap_or(0);
        let newest = self.data.source_epoch.get(source_key).copied().unwrap_or(0);
        let stored = self.data.source_stored_epoch.get(source_key).copied();
        if result_id < floor
            || result_id < newest
            || stored.is_some_and(|stored_id| result_id < stored_id)
        {
            return;
        }
        self.data.source_errors.insert(
            source_key.to_string(),
            error.unwrap_or("source request failed").to_string(),
        );
    }

    /// Effect batches resolve concurrently, so an older shared-dataset
    /// snapshot can arrive after a newer one; only the freshest may land.
    fn dataset_is_latest(&mut self, key: &'static str, result_id: u64) -> bool {
        let latest = self.data.dataset_epoch.entry(key).or_insert(0);
        if result_id >= *latest {
            *latest = result_id;
            true
        } else {
            false
        }
    }

    /// The command-result arms (`InvocationDispatch` / `ThreadCommandSubmitted` /
    /// `Invoked`) — a synthetic-`Null`-defaulted body that always resolves to a
    /// notice plus a refresh, never to the parse-and-store path.
    fn apply_invocation_result(
        &mut self,
        expected: &RyeOsEffectKind,
        kind: RyeOsEffectResultKind,
        data: serde_json::Value,
    ) -> Vec<RyeOsEffect> {
        match kind {
            RyeOsEffectResultKind::InvocationDispatch => {
                self.notice(effect_success_notice(expected, &data), RyeOsTone::Good);
                vec![
                    self.emit(RyeOsEffectKind::FetchDimension),
                    self.emit(RyeOsEffectKind::FetchThreads { limit: 100 }),
                ]
            }
            RyeOsEffectResultKind::ThreadCommandSubmitted => {
                self.notice(effect_success_notice(expected, &data), RyeOsTone::Good);
                let mut effects = vec![self.emit(RyeOsEffectKind::FetchThreads { limit: 200 })];
                effects.extend(self.effects_for_hint("thread"));
                effects
            }
            RyeOsEffectResultKind::Invoked => {
                // Typed submit result: { thread_id?, delivery, notice?, execution? }.
                let outcome: super::dto::LaunchOutcome =
                    serde_json::from_value(data.clone()).unwrap_or_default();
                self.apply_launch_outcome(expected, outcome, &data)
            }
            _ => unreachable!(),
        }
    }

    /// The `Invoked` tower: a plain service-ref success (row management), a
    /// refused/submitted live delivery, or a fresh launch that ratchets the
    /// seat route onto the produced thread.
    fn apply_launch_outcome(
        &mut self,
        expected: &RyeOsEffectKind,
        outcome: super::dto::LaunchOutcome,
        data: &serde_json::Value,
    ) -> Vec<RyeOsEffect> {
        // A `Service`-intent invoke (row management like cancel) is NOT a launch:
        // the emit site declared its intent, so the result never sniffs the ref.
        // Reading its result as a launch outcome would clear the focused filter
        // and falsely claim "Thread launched" — so handle it as a plain service
        // success: refresh the list/braid and preserve the focused input. Copy
        // comes from the affordance's `notice:` template (rendered against the
        // outcome), falling back to the generic success notice.
        if let RyeOsEffectKind::Invoke {
            intent: super::effect::InvokeIntent::Service,
            success_notice,
            ..
        } = expected
        {
            if outcome.delivery.is_none() {
                let notice = match success_notice {
                    Some(template) => render_result_notice(template, data),
                    None => effect_success_notice(expected, data),
                };
                self.notice(notice, RyeOsTone::Good);
                let mut effects = vec![self.emit(RyeOsEffectKind::FetchThreads { limit: 200 })];
                effects.extend(self.effects_for_hint("thread"));
                return effects;
            }
        }
        if outcome.delivery == Some(super::dto::ThreadDelivery::Refused) {
            // A refused delivery (non-continuation target, settled
            // status, or duplicate-submit conflict) delivered
            // nothing: KEEP the buffer so the operator's text isn't
            // lost, surface the daemon's reason, and do NOT ratchet
            // or claim a launch. (`thread_id` may be null or an
            // existing id; either way nothing new was created.)
            self.notice(
                outcome.notice.unwrap_or_else(|| REFUSED_NOTICE.to_string()),
                RyeOsTone::Warn,
            );
            return Vec::new();
        }
        if outcome.delivery == Some(super::dto::ThreadDelivery::Submitted) {
            // Live fold into a RUNNING thread: the stimulus was
            // delivered as a new cognition_in on the SAME thread — no
            // new thread, so no ratchet and no "launched" copy. Clear
            // the buffer and keep the route where it is; the live tail
            // shows the folded turn.
            self.clear_focused_input();
            let notice = submitted_delivery_notice(
                outcome.thread_id.as_deref(),
                outcome.notice,
                outcome.pending,
            );
            if notice.queued {
                self.notice_replacing_prefix(
                    INPUT_QUEUE_NOTICE_PREFIX,
                    notice.message,
                    notice.tone,
                );
            } else {
                self.notice(notice.message, notice.tone);
            }
            let mut effects = vec![self.emit(RyeOsEffectKind::FetchThreads { limit: 200 })];
            effects.extend(self.effects_for_hint("thread"));
            return effects;
        }
        self.clear_focused_input();
        let Some(thread_id) = outcome.thread_id.clone() else {
            self.notice(effect_success_notice(expected, data), RyeOsTone::Good);
            self.bump_generation();
            return Vec::new();
        };
        // Ratchet: the route is live state — a launch retargets
        // the input at the produced thread so the next submit
        // continues the chain. A stale result (route changed
        // since issue) may notice but never retargets.
        if let RyeOsEffectKind::Invoke {
            route_seq,
            ratchet_on_thread_id,
            ..
        } = expected
        {
            self.try_ratchet_route(
                *route_seq,
                *ratchet_on_thread_id,
                &thread_id,
                outcome.execution,
            );
        }
        self.notice(format!("Thread {thread_id} launched."), RyeOsTone::Good);
        let mut effects = vec![self.emit(RyeOsEffectKind::FetchThreads { limit: 200 })];
        effects.extend(self.effects_for_facet(super::seat::KEY_INPUT_ROUTE));
        effects.extend(self.effects_for_hint("thread"));
        effects
    }

    /// Retarget the seat route onto a just-launched thread, honoring the
    /// issue-time ratchet eligibility and the produced thread's substrate
    /// facts. Returns whether the route was retargeted; a stale result (the
    /// route moved since submit) notices and leaves the route untouched.
    fn try_ratchet_route(
        &mut self,
        route_seq: Option<u64>,
        ratchet_on_thread_id: bool,
        thread_id: &str,
        execution: Option<super::dto::ExecutionFacts>,
    ) -> bool {
        // Eligibility was decided at issue time (see submit_route)
        // — read it, don't recompute from current focus, which
        // may have moved while the launch was in flight. AND in
        // the produced thread's substrate facts when the result
        // carries them (an operator continuation does; a fresh
        // async launch doesn't — unknown stays eligible, and the
        // daemon refuses a real non-continuation continue).
        // Operator-input targeting: ratchet only onto a successor
        // that accepts OPERATOR follow-up (a graph continues by
        // machine but takes no operator input).
        let result_supports = execution.map(|e| e.supports_operator_followup);
        let targets = ratchet_on_thread_id && result_supports != Some(false);
        let fold = self.seat.fold();
        if fold.seq_of(super::seat::KEY_INPUT_ROUTE) != route_seq {
            self.notice(
                "Route changed since submit; not retargeting.",
                RyeOsTone::Warn,
            );
            return false;
        }
        // Only ratchet a continuation target onto routes
        // whose input declares conversation targeting. A
        // fire-and-forget route that happens to produce a
        // thread_id must NOT be retargeted as "continuing" —
        // same declaration the cycle and the label key off.
        if !targets {
            return false;
        }
        let mut route = fold.input_route();
        // First turn of a conversation: the launched
        // thread IS the chain root (root == head).
        // Continuations (route already had a head) keep
        // the root and only advance the head — so the
        // feed keeps showing the whole braid while the
        // next submit braids onto the newest turn.
        if route.thread.is_none() {
            route.chain_root = Some(thread_id.to_string());
        }
        route.thread = Some(thread_id.to_string());
        if let Ok(value) = serde_json::to_value(&route) {
            self.seat.append_facet(super::seat::KEY_INPUT_ROUTE, value);
        }
        true
    }

    /// The parse-and-store arms: deserialize the optional body into its DTO
    /// and fold it into `data`, honoring per-tile freshness/scope guards.
    fn apply_source_result(
        &mut self,
        expected: &RyeOsEffectKind,
        kind: RyeOsEffectResultKind,
        result_id: u64,
        data: serde_json::Value,
    ) -> Vec<RyeOsEffect> {
        match kind {
            RyeOsEffectResultKind::Dimension => {
                if self.dataset_is_latest("dimension", result_id) {
                    self.apply_parsed::<RyeOsDimensionDto>(data, "dimension", |core, dimension| {
                        core.data.dimension = Some(dimension);
                    });
                }
            }
            RyeOsEffectResultKind::SourceData => {
                if let RyeOsEffectKind::FetchSource { tile_id, .. } = expected {
                    // Freshness guard, two clauses:
                    // - the floor refuses stragglers from before the key's
                    //   subject changed (lens swap, drill return, selection
                    //   facet write) — mixed-subject data can never land;
                    // - within one subject, responses land MONOTONICALLY
                    //   against what is stored. Requiring the NEWEST request
                    //   here instead would starve any view whose query
                    //   latency exceeds the hint-refetch cadence into a
                    //   permanent "loading" — every response would arrive
                    //   already superseded.
                    let floor = self
                        .data
                        .source_floor
                        .get(tile_id)
                        .copied()
                        .unwrap_or(0);
                    let stored = self.data.source_stored_epoch.get(tile_id).copied();
                    if result_id >= floor && stored.is_none_or(|s| result_id >= s) {
                        let old = self.data.sources.get(tile_id).cloned();
                        self.note_source_row_changes(tile_id, old.as_ref(), &data);
                        self.data.sources.insert(tile_id.clone(), data);
                        self.data.source_errors.remove(tile_id);
                        self.data
                            .source_stored_epoch
                            .insert(tile_id.clone(), result_id);
                        self.rebuild_timeline_source_cache(tile_id);
                    }
                    self.bump_generation();
                }
            }
            RyeOsEffectResultKind::Projects => {
                if self.dataset_is_latest("projects", result_id) {
                    self.apply_parsed::<super::dto::RyeOsProjectsDto>(
                        data,
                        "projects",
                        |core, projects| {
                            core.data.projects = Some(projects);
                        },
                    );
                }
            }
            RyeOsEffectResultKind::Topology => {
                if self.dataset_is_latest("topology", result_id) {
                    self.apply_parsed::<RyeOsTopologyDto>(data, "topology", |core, topology| {
                        core.data.topology = Some(topology);
                    });
                }
            }
            RyeOsEffectResultKind::InvocationDispatch
            | RyeOsEffectResultKind::ThreadCommandSubmitted
            | RyeOsEffectResultKind::Invoked => {
                unreachable!("command results are handled before optional data extraction")
            }
            RyeOsEffectResultKind::ProjectAdded => {
                let added = match serde_json::from_value::<RyeOsAddProjectDto>(data) {
                    Ok(added) => added,
                    Err(error) => {
                        self.notice(
                            format!("RyeOS could not read project_add response: {error}"),
                            RyeOsTone::Danger,
                        );
                        return Vec::new();
                    }
                };
                self.notice(
                    if added.created {
                        format!("Registered project {}.", added.project.name)
                    } else {
                        format!("Updated project {}.", added.project.name)
                    },
                    RyeOsTone::Good,
                );
                return vec![self.emit(RyeOsEffectKind::FetchProjects)];
            }
            RyeOsEffectResultKind::ProjectOpened => {
                return self.apply_project_opened(data);
            }
            RyeOsEffectResultKind::Threads => {
                if self.dataset_is_latest("threads", result_id) {
                    self.apply_parsed::<RyeOsThreadsDto>(data, "threads", |core, threads| {
                        core.data.threads = Some(threads);
                    });
                }
            }
            RyeOsEffectResultKind::Items => {
                self.apply_parsed::<RyeOsItemsDto>(data, "items", |core, items| {
                    if effect_matches_current_items(Some(expected), core) {
                        if let RyeOsEffectKind::FetchItems {
                            tile_id: Some(tile_id),
                            ..
                        } = expected
                        {
                            core.data.tile_items.insert(tile_id.clone(), items.clone());
                        } else if core.dataset_is_latest("items", result_id) {
                            core.data.items = Some(items);
                        }
                    }
                });
            }
            RyeOsEffectResultKind::FilesList => {
                self.apply_parsed::<RyeOsFilesDto>(data, "files_list", |core, files| {
                    if effect_matches_current_files(Some(expected), core, &files) {
                        if let RyeOsEffectKind::ListFiles {
                            tile_id: Some(tile_id),
                            ..
                        } = expected
                        {
                            core.data.tile_files.insert(tile_id.clone(), files.clone());
                        }
                        core.data.files = Some(files);
                    }
                });
            }
            RyeOsEffectResultKind::FileSpace => {
                self.apply_parsed::<RyeOsFileSpaceDto>(data, "file_space", |core, file_space| {
                    if effect_matches_current_file_space(Some(expected), core, &file_space) {
                        if let RyeOsEffectKind::FetchFileSpace {
                            tile_id: Some(tile_id),
                            ..
                        } = expected
                        {
                            core.data
                                .tile_file_space
                                .insert(tile_id.clone(), file_space);
                        } else {
                            core.data.file_space = Some(file_space);
                        }
                    }
                });
            }
            RyeOsEffectResultKind::FileRead => {
                self.apply_parsed::<RyeOsFileReadDto>(data, "file_read", |core, file_read| {
                    if effect_matches_current_file_read(Some(expected), core, &file_read) {
                        core.data.file_read = Some(file_read);
                    }
                });
            }
            RyeOsEffectResultKind::BrowserOnly => {}
        }

        self.bump_generation();
        Vec::new()
    }

    fn apply_project_opened(&mut self, data: serde_json::Value) -> Vec<RyeOsEffect> {
        let opened = match serde_json::from_value::<RyeOsOpenProjectDto>(data) {
            Ok(opened) => opened,
            Err(error) => {
                self.notice(
                    format!("RyeOS could not read project_open response: {error}"),
                    RyeOsTone::Danger,
                );
                return Vec::new();
            }
        };
        let project_root =
            opened.session.project_root.clone().or_else(|| {
                (!opened.project.root.is_empty()).then_some(opened.project.root.clone())
            });
        if let Some(session) = &mut self.data.session {
            if !opened.session.session_id.is_empty() {
                session.session_id = opened.session.session_id.clone();
                session.read_only = opened.session.read_only;
            }
            session.project_path = project_root;
        }
        if let Some(projects) = &mut self.data.projects {
            for project in &mut projects.projects {
                project.current = project.local_id == opened.project.local_id;
                if project.current {
                    *project = opened.project.clone();
                    project.current = true;
                }
            }
        }
        self.data.dimension = None;
        self.data.topology = None;
        self.data.items = None;
        self.data.file_space = None;
        self.data.tile_items.clear();
        self.data.tile_file_space.clear();
        self.data.files = None;
        self.data.tile_files.clear();
        self.data.sources.clear();
        self.data.source_errors.clear();
        self.data.source_epoch.clear();
        self.data.source_stored_epoch.clear();
        self.data.source_floor.clear();
        self.deferred_source_fetches.clear();
        self.data.timeline_sources.clear();
        self.data.file_read = None;
        self.pending_effects
            .retain(|_, kind| !effect_depends_on_project_binding(kind));
        self.notice(
            format!("Opened project {}.", opened.project.name),
            RyeOsTone::Good,
        );
        self.initial_effects()
    }

    pub(crate) fn apply_parsed<T>(
        &mut self,
        data: serde_json::Value,
        label: &'static str,
        apply: impl FnOnce(&mut Self, T),
    ) where
        T: serde::de::DeserializeOwned,
    {
        match serde_json::from_value::<T>(data) {
            Ok(value) => apply(self, value),
            Err(error) => self.notice(
                format!("RyeOS could not read {label} response: {error}"),
                RyeOsTone::Danger,
            ),
        }
    }
}

struct SubmittedDeliveryNotice {
    message: String,
    tone: RyeOsTone,
    queued: bool,
}

fn submitted_delivery_notice(
    thread_id: Option<&str>,
    daemon_notice: Option<String>,
    pending: Option<u64>,
) -> SubmittedDeliveryNotice {
    if let Some(pending) = pending.filter(|pending| *pending > 0) {
        return SubmittedDeliveryNotice {
            message: format_staged_input_notice(pending),
            tone: RyeOsTone::Accent,
            queued: true,
        };
    }

    if let Some(notice) = daemon_notice {
        let queued = notice.starts_with("Input queued");
        return SubmittedDeliveryNotice {
            message: if queued {
                notice.replace("Input queued", INPUT_QUEUE_NOTICE_PREFIX)
            } else {
                notice
            },
            tone: if queued {
                RyeOsTone::Accent
            } else {
                RyeOsTone::Warn
            },
            queued,
        };
    }

    SubmittedDeliveryNotice {
        message: match thread_id {
            Some(id) => format!("Input delivered to {id}."),
            None => "Input delivered.".to_string(),
        },
        tone: RyeOsTone::Good,
        queued: false,
    }
}

fn format_staged_input_notice(pending: u64) -> String {
    let label = if pending == 1 { "input" } else { "inputs" };
    format!("{INPUT_QUEUE_NOTICE_PREFIX} · {pending} staged {label}.")
}

/// Fallback notice when a refused delivery carries no reason from the daemon.
const REFUSED_NOTICE: &str = "Delivery refused.";

fn effect_success_notice(expected: &RyeOsEffectKind, data: &serde_json::Value) -> String {
    match expected {
        RyeOsEffectKind::DispatchInvocation { item_ref, .. } => {
            // `data.target.ref` is a path, not key alternatives — reaching
            // for `json_field_text` here would stringify the whole target.
            let item_ref = data
                .get("target")
                .and_then(|target| target.get("ref"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| item_ref.clone());
            format!("Ran {item_ref}.")
        }
        RyeOsEffectKind::SubmitThreadCommand {
            thread_id,
            command_type,
        } => format!("Sent {command_type} to {thread_id}."),
        RyeOsEffectKind::Invoke { .. } => "Invocation completed.".to_string(),
        _ => "RyeOS command completed.".to_string(),
    }
}

fn effect_failure_notice(expected: &RyeOsEffectKind, error: Option<&str>) -> String {
    let reason = error
        .and_then(effect_error_summary)
        .unwrap_or_else(|| "RyeOS platform effect failed".to_string());
    match expected {
        RyeOsEffectKind::DispatchInvocation { item_ref, .. } => {
            format!("Run {item_ref} failed: {reason}")
        }
        RyeOsEffectKind::SubmitThreadCommand {
            thread_id,
            command_type,
        } => format!("Sending {command_type} to {thread_id} failed: {reason}"),
        RyeOsEffectKind::Invoke { .. } => format!("Invocation failed: {reason}"),
        _ => reason,
    }
}

fn effect_error_summary(raw: &str) -> Option<String> {
    structured_error_message(raw).or_else(|| {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn structured_error_message(raw: &str) -> Option<String> {
    raw.char_indices()
        .filter_map(|(index, ch)| (ch == '{').then_some(index))
        .find_map(|index| serde_json::from_str::<serde_json::Value>(&raw[index..]).ok())
        .and_then(|value| {
            json_field_text(&value, &["message", "error", "detail", "code"]).or_else(|| {
                value
                    .get("body")
                    .and_then(|body| json_field_text(body, &["message", "error", "detail", "code"]))
            })
        })
}

/// Render an affordance success-notice template, substituting `{result.<field>}`
/// tokens with the matching field of the invocation outcome (the result body).
fn render_result_notice(template: &str, data: &serde_json::Value) -> String {
    const OPEN: &str = "{result.";
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        match after.find('}') {
            Some(end) => {
                let field = &after[..end];
                out.push_str(&json_field_text(data, &[field]).unwrap_or_default());
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn json_field_text(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| value.get(*key)).map(|v| {
        v.as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string())
    })
}

fn effect_result_kind_matches(expected: &RyeOsEffectKind, actual: &RyeOsEffectResultKind) -> bool {
    matches!(
        (expected, actual),
        (
            RyeOsEffectKind::FetchDimension,
            RyeOsEffectResultKind::Dimension
        ) | (
            RyeOsEffectKind::FetchProjects,
            RyeOsEffectResultKind::Projects
        ) | (
            RyeOsEffectKind::FetchTopology,
            RyeOsEffectResultKind::Topology
        ) | (
            RyeOsEffectKind::AddProject { .. },
            RyeOsEffectResultKind::ProjectAdded
        ) | (
            RyeOsEffectKind::OpenProject { .. },
            RyeOsEffectResultKind::ProjectOpened
        ) | (
            RyeOsEffectKind::FetchThreads { .. },
            RyeOsEffectResultKind::Threads
        ) | (
            RyeOsEffectKind::FetchItems { .. },
            RyeOsEffectResultKind::Items
        ) | (
            RyeOsEffectKind::FetchSource { .. },
            RyeOsEffectResultKind::SourceData
        ) | (
            RyeOsEffectKind::ListFiles { .. },
            RyeOsEffectResultKind::FilesList
        ) | (
            RyeOsEffectKind::FetchFileSpace { .. },
            RyeOsEffectResultKind::FileSpace
        ) | (
            RyeOsEffectKind::ReadFile { .. },
            RyeOsEffectResultKind::FileRead
        ) | (
            RyeOsEffectKind::DispatchInvocation { .. },
            RyeOsEffectResultKind::InvocationDispatch
        ) | (
            RyeOsEffectKind::SubmitThreadCommand { .. },
            RyeOsEffectResultKind::ThreadCommandSubmitted
        ) | (
            RyeOsEffectKind::Invoke { .. },
            RyeOsEffectResultKind::Invoked
        ) | (
            RyeOsEffectKind::SetLocationHash { .. },
            RyeOsEffectResultKind::BrowserOnly
        ) | (
            RyeOsEffectKind::CopyToClipboard { .. },
            RyeOsEffectResultKind::BrowserOnly
        ) | (
            RyeOsEffectKind::OpenUrl { .. },
            RyeOsEffectResultKind::BrowserOnly
        )
    )
}

fn effect_depends_on_project_binding(kind: &RyeOsEffectKind) -> bool {
    matches!(
        kind,
        RyeOsEffectKind::FetchDimension
            | RyeOsEffectKind::FetchTopology
            | RyeOsEffectKind::FetchItems { .. }
            | RyeOsEffectKind::FetchFileSpace { .. }
            | RyeOsEffectKind::ListFiles { .. }
            | RyeOsEffectKind::ReadFile { .. }
            | RyeOsEffectKind::DispatchInvocation { .. }
            | RyeOsEffectKind::FetchSource { .. }
    )
}

fn effect_matches_current_items(expected: Option<&RyeOsEffectKind>, core: &RyeOsCore) -> bool {
    let Some(RyeOsEffectKind::FetchItems {
        tile_id,
        query,
        kind,
        ..
    }) = expected
    else {
        return true;
    };
    let Some(tile_id) = tile_id.as_deref() else {
        // The shared/ambient fetch is unscoped.
        return query.is_none() && kind.is_none();
    };
    // A tile-scoped fetch is current iff that tile still binds an atlas view
    // whose declared `body.scope` still matches this fetch's (query, kind) —
    // content is the scope, so a re-bound or re-scoped tile drops the stale
    // response instead of caching it.
    let Some(tile_id) = parse_tile_id(tile_id) else {
        return false;
    };
    let Some(binding) = core
        .workspace
        .tiles
        .get(&tile_id)
        .and_then(|tile| core.views.get(&tile.view.view_ref))
    else {
        return false;
    };
    if binding.widget != "atlas" {
        return false;
    }
    let (want_query, want_kind) = super::model::atlas_item_scope(binding);
    &want_query == query && &want_kind == kind
}

fn effect_matches_current_files(
    expected: Option<&RyeOsEffectKind>,
    core: &RyeOsCore,
    files: &RyeOsFilesDto,
) -> bool {
    let Some(RyeOsEffectKind::ListFiles {
        tile_id,
        root,
        path,
    }) = expected
    else {
        return true;
    };
    let Some(tile_id) = tile_id.as_deref().and_then(parse_tile_id) else {
        return false;
    };
    // Tile-scoped file listings died with the legacy file tiles.
    let _ = (core, tile_id, root, path, files);
    false
}

fn effect_matches_current_file_space(
    expected: Option<&RyeOsEffectKind>,
    core: &RyeOsCore,
    file_space: &RyeOsFileSpaceDto,
) -> bool {
    let Some(RyeOsEffectKind::FetchFileSpace {
        tile_id,
        root,
        path,
        ..
    }) = expected
    else {
        return true;
    };
    let response_matches = root == &file_space.root && path == &file_space.path;
    // Per-tile fetch: validate against THIS tile's file-space arrangement;
    // the shared fetch validates against the ambient atlas state.
    let atlas = match tile_id.as_deref().map(parse_tile_id) {
        Some(Some(tile_id)) => core.tile_atlas_state(tile_id),
        Some(None) => return false,
        None => &core.ui.atlas,
    };
    atlas.active_projection.is_file_space()
        && root == &atlas.file_space_root
        && path == &atlas.file_space_path
        && response_matches
}

fn effect_matches_current_file_read(
    expected: Option<&RyeOsEffectKind>,
    core: &RyeOsCore,
    file_read: &RyeOsFileReadDto,
) -> bool {
    let Some(RyeOsEffectKind::ReadFile { root, path }) = expected else {
        return true;
    };
    let selection_matches = core
        .seat
        .fold()
        .get(super::seat::KEY_SELECTION)
        .and_then(|selection| selection.get("file"))
        .is_some_and(|file| {
            file.get("root") == Some(&serde_json::Value::String(root.clone()))
                && file.get("path") == Some(&serde_json::Value::String(path.clone()))
        });
    if !selection_matches {
        return false;
    }
    root == &file_read.root && path == &file_read.path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::reducer::test_support::*;

    #[test]
    fn service_ref_cancel_result_refreshes_without_launch_semantics() {
        use crate::ui::effect::{InvokeRef, RyeOsEffectResult, RyeOsEffectResultKind};
        // A focused list lens with a live filter and typed text.
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": ["view:ryeos/threads/list"],
                "views": { "view:ryeos/threads/list": {
                    "widget": "table",
                    "source": { "ref": "service:ui/ryeos-ui/threads/list", "params": {}, "collection": "threads" },
                    "input": { "id": "filter", "feeds": { "param": "status" } }
                }}
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::InsertInputChar { ch: 'r' },
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::InsertInputChar { ch: 'u' },
        });
        assert_eq!(
            core.focused_input_buffer().map(|b| b.text.clone()),
            Some("ru".to_string())
        );

        // A service-ref cancel invoke (as the row Cancel affordance emits) whose
        // result looks superficially like a launch outcome ({thread_id, status}).
        let effect = core.emit(RyeOsEffectKind::Invoke {
            target: InvokeRef::Ref {
                item_ref: "service:commands/submit".to_string(),
            },
            params: serde_json::json!({ "thread_id": "T-7", "command_type": "cancel" }),
            // As the row Cancel affordance now emits: a Service intent carrying
            // the authored success-notice template.
            intent: crate::ui::effect::InvokeIntent::Service,
            success_notice: Some("Cancelled {result.thread_id}.".to_string()),
            route_seq: None,
            ratchet_on_thread_id: false,
        });
        let effects = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::Invoked,
                data: Some(serde_json::json!({ "thread_id": "T-7", "status": "cancelled" })),
                error: None,
            },
        });

        // The focused filter buffer is PRESERVED (a launch would have cleared it).
        assert_eq!(
            core.focused_input_buffer().map(|b| b.text.clone()),
            Some("ru".to_string()),
            "cancel must not clear the focused filter"
        );
        // The route is NOT ratcheted onto the cancelled thread.
        assert!(core.seat.fold().input_route().thread.is_none());
        // A thread-list refresh is emitted.
        assert!(
            effects
                .iter()
                .any(|e| matches!(&e.kind, RyeOsEffectKind::FetchThreads { .. })),
            "cancel refreshes the thread list; got {effects:?}"
        );
        // The notice reads as a cancel, not a false launch.
        let notice = core.ui.notices.last().expect("a notice");
        assert!(
            notice.message.contains("Cancelled"),
            "got {:?}",
            notice.message
        );
        assert!(
            !notice.message.contains("launched"),
            "got {:?}",
            notice.message
        );
    }

    #[test]
    fn topology_effect_result_updates_state() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        let effects = core.initial_effects();
        let topology_id = effects
            .iter()
            .find(|effect| matches!(effect.kind, RyeOsEffectKind::FetchTopology))
            .map(|effect| effect.id)
            .expect("graph startup should fetch topology");

        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: topology_id,
                ok: true,
                kind: RyeOsEffectResultKind::Topology,
                data: Some(serde_json::json!({
                    "version": "1",
                    "kind": "topology",
                    "metadata": {},
                    "nodes": [{
                        "id": "tool:demo/run",
                        "kind": "tool",
                        "label": "run",
                        "ref": "tool:demo/run"
                    }],
                    "edges": []
                })),
                error: None,
            },
        });

        let topology = core.data.topology.as_ref().expect("topology state");
        assert_eq!(topology.nodes.len(), 1);
        assert_eq!(topology.nodes[0].ref_, "tool:demo/run");
    }

    #[test]
    fn stale_source_response_is_dropped_by_the_freshness_guard() {
        use crate::ui::effect::{RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind};
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        // Two fetches issued for the SAME key — a single-lens tile reused for a
        // new selection keeps its key. The second is the newest request.
        let older = core.emit(RyeOsEffectKind::FetchSource {
            tile_id: "K".to_string(),
            source_ref: "service:x".to_string(),
            params: serde_json::json!({}),
        });
        let newer = core.emit(RyeOsEffectKind::FetchSource {
            tile_id: "K".to_string(),
            source_ref: "service:x".to_string(),
            params: serde_json::json!({}),
        });
        assert!(newer.id > older.id);
        // build_fetch_source would record the newest request; simulate that.
        core.data.source_epoch.insert("K".to_string(), newer.id);

        let deliver = |core: &mut RyeOsCore, id: u64, tag: &str| {
            core.dispatch(RyeOsEvent::EffectResult {
                result: RyeOsEffectResult {
                    id,
                    ok: true,
                    kind: RyeOsEffectResultKind::SourceData,
                    data: Some(serde_json::json!({ "tag": tag })),
                    error: None,
                },
            });
        };

        // Newest resolves first and lands.
        deliver(&mut core, newer.id, "new");
        assert_eq!(core.data.sources["K"]["tag"], "new");
        // An older straggler resolving afterwards is DROPPED — a slow fetch for
        // the previous selection must not overwrite the current one.
        deliver(&mut core, older.id, "old");
        assert_eq!(
            core.data.sources["K"]["tag"], "new",
            "stale straggler must not overwrite the newest response"
        );
    }

    /// Deliver a `SourceData` result carrying `{ "tag": <tag> }`.
    fn deliver(core: &mut RyeOsCore, id: u64, tag: &str) {
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id,
                ok: true,
                kind: RyeOsEffectResultKind::SourceData,
                data: Some(serde_json::json!({ "tag": tag })),
                error: None,
            },
        });
    }

    #[test]
    fn superseded_first_response_still_lands_when_nothing_is_stored() {
        // A refetch cadence faster than the query latency must not starve
        // the view: the FIRST response to arrive renders even though a
        // newer request is already in flight; the newer response then
        // replaces it monotonically.
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/slow");
        let older = core
            .emit_fetch_source_keyed("K".to_string(), "view:test/slow")
            .pop()
            .expect("fetch emitted");
        let newer = core
            .emit_fetch_source_keyed("K".to_string(), "view:test/slow")
            .pop()
            .expect("fetch emitted");
        assert!(newer.id > older.id);

        deliver(&mut core, older.id, "first");
        assert_eq!(
            core.data.sources["K"]["tag"], "first",
            "superseded-but-first response must render, not starve"
        );
        deliver(&mut core, newer.id, "second");
        assert_eq!(core.data.sources["K"]["tag"], "second");
    }

    #[test]
    fn source_failure_is_visible_until_retry_or_success() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/slow");
        let failed = core
            .emit_fetch_source_keyed("K".to_string(), "view:test/slow")
            .pop()
            .expect("fetch emitted");

        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: failed.id,
                ok: false,
                kind: RyeOsEffectResultKind::SourceData,
                data: None,
                error: Some("daemon rejected source".to_string()),
            },
        });
        assert_eq!(
            core.data.source_errors.get("K").map(String::as_str),
            Some("daemon rejected source")
        );

        let retry = core
            .emit_fetch_source_keyed("K".to_string(), "view:test/slow")
            .pop()
            .expect("retry emitted");
        assert!(!core.data.source_errors.contains_key("K"));

        deliver(&mut core, retry.id, "recovered");
        assert_eq!(core.data.sources["K"]["tag"], "recovered");
        assert!(!core.data.source_errors.contains_key("K"));
    }

    #[test]
    fn superseded_source_failure_does_not_hide_newer_request() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/slow");
        let older = core
            .emit_fetch_source_keyed("K".to_string(), "view:test/slow")
            .pop()
            .expect("older fetch emitted");
        let newer = core
            .emit_fetch_source_keyed("K".to_string(), "view:test/slow")
            .pop()
            .expect("newer fetch emitted");

        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: older.id,
                ok: false,
                kind: RyeOsEffectResultKind::SourceData,
                data: None,
                error: Some("stale failure".to_string()),
            },
        });
        assert!(!core.data.source_errors.contains_key("K"));

        deliver(&mut core, newer.id, "new");
        assert_eq!(core.data.sources["K"]["tag"], "new");
    }

    #[test]
    fn facet_write_floor_refuses_pre_write_stragglers() {
        // Even with the store empty after eviction, a response from a fetch
        // issued BEFORE the facet write (the old subject) must not land.
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/detail",
            serde_json::json!({
                "widget": "rows",
                "refresh": { "on_facet": "selection" },
                "source": { "ref": "service:test/detail", "params": {}, "collection": "rows" }
            }),
        );
        let tile_id = core.workspace.add_tile(ViewSpec {
            view_ref: "view:test/detail".to_string(),
        });
        let key = tile_id.0.to_string();
        let pre_write = core
            .emit_fetch_source_keyed(key.clone(), "view:test/detail")
            .pop()
            .expect("fetch emitted");

        let refetch = core.effects_for_facet("selection");
        assert!(!refetch.is_empty(), "facet write refetches the subscriber");

        deliver(&mut core, pre_write.id, "old-subject");
        assert!(
            core.data.sources.get(&key).is_none(),
            "pre-write straggler must be refused by the floor"
        );
        let fresh = refetch.last().expect("refetch effect");
        deliver(&mut core, fresh.id, "new-subject");
        assert_eq!(core.data.sources[&key]["tag"], "new-subject");
    }

    #[test]
    fn refetching_a_sections_view_keeps_prior_data_while_pending() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/detail",
            serde_json::json!({
                "widget": "sections",
                "sections": [
                    { "title": "A", "source": { "ref": "service:a" }, "projection": {} },
                    { "title": "B", "source": { "ref": "service:b" }, "projection": {} }
                ]
            }),
        );
        let k0 = crate::ui::content::section_source_key("K", 0);
        let k1 = crate::ui::content::section_source_key("K", 1);
        core.data
            .sources
            .insert(k0.clone(), serde_json::json!({ "stale": "A" }));
        core.data
            .sources
            .insert(k1.clone(), serde_json::json!({ "stale": "B" }));

        // Refetching keeps each section's prior response rendering while the
        // fresh fetches are in flight — hint-driven activity refreshes would
        // otherwise blank the view every coalesced tick. Staleness is the
        // epoch guard's job (see the out-of-order straggler test above), not
        // the emitter's.
        let effects = core.emit_fetch_source_keyed("K".to_string(), "view:test/detail");
        assert!(core.data.sources.contains_key(&k0));
        assert!(core.data.sources.contains_key(&k1));
        assert_eq!(effects.len(), 2, "one fetch per section");
    }

    #[test]
    fn facet_write_evicts_prior_section_data_for_subscribed_views() {
        // The counterpart guard to keeps-prior above: a facet write means
        // the SUBJECT changed, and the old subject's sections must never
        // render underneath the new selection — even if the refetch is
        // skipped or fails.
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/detail",
            serde_json::json!({
                "widget": "sections",
                "refresh": { "on_facet": "selection" },
                "sections": [
                    { "title": "A", "source": { "ref": "service:a" }, "projection": {} },
                    { "title": "B", "source": { "ref": "service:b" }, "projection": {} }
                ]
            }),
        );
        let tile_id = core.workspace.add_tile(ViewSpec {
            view_ref: "view:test/detail".to_string(),
        });
        let key = tile_id.0.to_string();
        let k0 = crate::ui::content::section_source_key(&key, 0);
        let k1 = crate::ui::content::section_source_key(&key, 1);
        core.data
            .sources
            .insert(k0.clone(), serde_json::json!({ "stale": "A" }));
        core.data
            .sources
            .insert(k1.clone(), serde_json::json!({ "stale": "B" }));

        let effects = core.effects_for_facet("selection");

        assert!(
            !core.data.sources.contains_key(&k0),
            "old subject's section payload must not survive a facet write"
        );
        assert!(!core.data.sources.contains_key(&k1));
        assert_eq!(effects.len(), 2, "one fresh fetch per section");
    }

    #[test]
    fn open_project_effect_result_rebinds_session_and_reloads() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenProject {
                    local_id: "prj_1".to_string(),
                },
            },
        });
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(RyeOsEffectKind::OpenProject { local_id }) if local_id == "prj_1"
        ));

        let reloads = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effects[0].id,
                ok: true,
                kind: RyeOsEffectResultKind::ProjectOpened,
                data: Some(serde_json::json!({
                    "project": {
                        "local_id": "prj_1",
                        "name": "next",
                        "root": "/tmp/next",
                        "exists": true
                    },
                    "session": {
                        "session_id": "session-1",
                        "project_root": "/tmp/next",
                        "read_only": false
                    },
                    "recent": []
                })),
                error: None,
            },
        });

        assert_eq!(
            core.data
                .session
                .as_ref()
                .and_then(|s| s.project_path.as_deref()),
            Some("/tmp/next")
        );
        assert!(reloads
            .iter()
            .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchDimension)));
        assert!(reloads
            .iter()
            .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchProjects)));
    }

    #[test]
    fn project_added_refetches_projects() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::AddCurrentProject,
            },
        });
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(RyeOsEffectKind::AddProject { root }) if root == "/tmp/project"
        ));

        let followups = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effects[0].id,
                ok: true,
                kind: RyeOsEffectResultKind::ProjectAdded,
                data: Some(serde_json::json!({
                    "project": {
                        "local_id": "prj_1",
                        "name": "project",
                        "root": "/tmp/project",
                        "exists": true
                    },
                    "created": true
                })),
                error: None,
            },
        });

        assert!(matches!(
            followups.first().map(|effect| &effect.kind),
            Some(RyeOsEffectKind::FetchProjects)
        ));
    }

    #[test]
    fn refused_delivery_surfaces_reason_and_does_not_ratchet() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core); // targeting input → ratchet would be eligible
        set_focused_input(&mut core, "go");
        let effect = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        // Daemon refuses (e.g. non-continuation target / duplicate conflict).
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::Invoked,
                data: Some(serde_json::json!({
                    "thread_id": serde_json::Value::Null,
                    "delivery": "refused",
                    "notice": "thread is not continuation-capable"
                })),
                error: None,
            },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(route.thread, None, "refused → no ratchet");
        assert_eq!(route.chain_root, None);
        assert!(
            core.ui
                .notices
                .iter()
                .any(|n| n.message.contains("not continuation-capable")),
            "surfaces the daemon's refusal reason, not a generic success"
        );
    }

    #[test]
    fn continuation_advances_head_but_preserves_chain_root() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);

        // Turn 1: starts the conversation. root == head == T-1.
        set_focused_input(&mut core, "hello");
        let e1 = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: e1.id,
                ok: true,
                kind: RyeOsEffectResultKind::Invoked,
                data: Some(serde_json::json!({ "thread_id": "T-1", "delivery": "launched" })),
                error: None,
            },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(route.thread.as_deref(), Some("T-1"));
        assert_eq!(route.chain_root.as_deref(), Some("T-1"));

        // Turn 2: a follow-up braids onto T-1 → new head T-2, same root.
        set_focused_input(&mut core, "and again");
        let e2 = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: e2.id,
                ok: true,
                kind: RyeOsEffectResultKind::Invoked,
                data: Some(serde_json::json!({ "thread_id": "T-2", "delivery": "launched" })),
                error: None,
            },
        });
        let route = core.seat.fold().input_route();
        // Head advanced to the new turn; the next submit braids onto it.
        assert_eq!(route.thread.as_deref(), Some("T-2"));
        // Root unchanged — the feed keeps showing the whole conversation.
        assert_eq!(route.chain_root.as_deref(), Some("T-1"));
    }

    #[test]
    fn stale_invoke_result_never_retargets_newer_route() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        set_focused_input(&mut core, "first");
        let effect = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");

        // Route changes after the submit was issued.
        core.seat.append_facet(
            crate::ui::seat::KEY_INPUT_ROUTE,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "thread": "T-other"
            }),
        );

        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::Invoked,
                data: Some(serde_json::json!({
                    "thread_id": "T-stale",
                    "delivery": "launched"
                })),
                error: None,
            },
        });

        let route = core.seat.fold().input_route();
        assert_eq!(route.thread.as_deref(), Some("T-other"));
    }

    #[test]
    fn refused_delivery_keeps_buffer() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        set_focused_input(&mut core, "hold on");
        let effect = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");

        let followups = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::Invoked,
                data: Some(serde_json::json!({
                    "delivery": "refused",
                    "notice": "Thread is live; delivery refused."
                })),
                error: None,
            },
        });

        assert!(followups.is_empty());
        assert_eq!(focused_input_text(&core), "hold on");
        assert!(core
            .ui
            .notices
            .last()
            .is_some_and(|notice| notice.message.contains("refused")));
    }

    #[test]
    fn invocation_dispatch_result_notices_and_refreshes() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        let invoke = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::ExecuteItem {
                    item_ref: "tool:demo/run".to_string(),
                    ref_bindings: std::collections::BTreeMap::new(),
                    parameters: serde_json::json!({}),
                },
            },
        });
        let invoke_id = invoke
            .first()
            .map(|effect| effect.id)
            .expect("execute should emit invoke effect");

        let effects = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: invoke_id,
                ok: true,
                kind: RyeOsEffectResultKind::InvocationDispatch,
                data: Some(serde_json::json!({
                    "status": "executed",
                    "target": { "kind": "ref", "ref": "tool:demo/run" },
                    "invocation_id": "inv-1"
                })),
                error: None,
            },
        });

        assert!(core
            .ui
            .notices
            .iter()
            .any(|notice| notice.message == "Ran tool:demo/run."));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchDimension)));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchThreads { limit: 100 })));
    }

    #[test]
    fn invocation_dispatch_failure_names_item_and_structured_error() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        let invoke = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::ExecuteItem {
                    item_ref: "tool:demo/run".to_string(),
                    ref_bindings: std::collections::BTreeMap::new(),
                    parameters: serde_json::json!({}),
                },
            },
        });
        let invoke_id = invoke
            .first()
            .map(|effect| effect.id)
            .expect("execute should emit invoke effect");

        let effects = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: invoke_id,
                ok: false,
                kind: RyeOsEffectResultKind::InvocationDispatch,
                data: None,
                error: Some(
                    "/ui/api/invocations/dispatch: 500 {\"message\":\"capability denied\"}"
                        .to_string(),
                ),
            },
        });

        assert!(effects.is_empty());
        assert!(core.ui.notices.iter().any(|notice| {
            notice.message == "Run tool:demo/run failed: capability denied"
                && notice.tone == RyeOsTone::Danger
        }));
    }

    #[test]
    fn invocation_dispatch_success_without_body_still_notices_and_refreshes() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        let invoke = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::ExecuteItem {
                    item_ref: "tool:demo/run".to_string(),
                    ref_bindings: std::collections::BTreeMap::new(),
                    parameters: serde_json::json!({}),
                },
            },
        });
        let invoke_id = invoke
            .first()
            .map(|effect| effect.id)
            .expect("execute should emit invoke effect");

        let effects = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: invoke_id,
                ok: true,
                kind: RyeOsEffectResultKind::InvocationDispatch,
                data: None,
                error: None,
            },
        });

        assert!(core
            .ui
            .notices
            .iter()
            .any(|notice| notice.message == "Ran tool:demo/run."));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchDimension)));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchThreads { limit: 100 })));
    }

    #[test]
    fn thread_command_submitted_result_notices_and_refreshes() {
        // The one cancel path's result: an Esc / command overlay / row cancel all land
        // as `SubmitThreadCommand { cancel }` → `ThreadCommandSubmitted`, which
        // notices and refreshes the thread list.
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        core.seat.append_facet(
            crate::ui::seat::KEY_INPUT_ROUTE,
            serde_json::json!({ "thread": "T-run" }),
        );
        core.data.threads = Some(RyeOsThreadsDto {
            threads: vec![serde_json::json!({ "thread_id": "T-run", "status": "running" })],
        });
        let cancel = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::InterruptHead,
        });
        let cancel_id = cancel
            .first()
            .map(|effect| effect.id)
            .expect("cancel should emit effect");

        let effects = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: cancel_id,
                ok: true,
                kind: RyeOsEffectResultKind::ThreadCommandSubmitted,
                data: Some(serde_json::json!({
                    "thread_id": "T-run",
                    "command_type": "cancel"
                })),
                error: None,
            },
        });

        assert!(core
            .ui
            .notices
            .iter()
            .any(|notice| notice.message == "Sent cancel to T-run."));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchThreads { limit: 200 })));
    }

    #[test]
    fn thread_cancel_failure_names_thread() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        core.seat.append_facet(
            crate::ui::seat::KEY_INPUT_ROUTE,
            serde_json::json!({ "thread": "T-run" }),
        );
        core.data.threads = Some(RyeOsThreadsDto {
            threads: vec![serde_json::json!({ "thread_id": "T-run", "status": "running" })],
        });
        let cancel = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::InterruptHead,
        });
        let cancel_id = cancel
            .first()
            .map(|effect| effect.id)
            .expect("cancel should emit effect");

        let effects = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: cancel_id,
                ok: false,
                kind: RyeOsEffectResultKind::ThreadCommandSubmitted,
                data: None,
                error: Some("thread already finished".to_string()),
            },
        });

        assert!(effects.is_empty());
        assert!(core.ui.notices.iter().any(|notice| {
            notice.message == "Sending cancel to T-run failed: thread already finished"
                && notice.tone == RyeOsTone::Danger
        }));
    }

    #[test]
    fn dimension_effect_result_updates_view_model() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        let effects = core.initial_effects();
        let dimension_id = effects
            .iter()
            .find(|effect| matches!(effect.kind, RyeOsEffectKind::FetchDimension))
            .map(|effect| effect.id)
            .expect("initial load should fetch dimension");
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: dimension_id,
                ok: true,
                kind: RyeOsEffectResultKind::Dimension,
                data: Some(serde_json::json!({
                    "schema_version": "ryeos.test",
                    "session": {
                        "session_id": "session-1",
                        "surface_ref": "surface:ryeos/ryeos/base",
                        "read_only": true
                    },
                    "local_node": {
                        "health": { "status": "healthy" },
                        "services": [
                            { "endpoint": "ui.session.current", "service_ref": "service:ui/session/current", "availability": "DaemonOnly" }
                        ]
                    },
                    "project": { "path": "/tmp/project" }
                })),
                error: None,
            },
        });

        let envelope = core.envelope(Vec::new());
        assert_eq!(envelope.view_model.chrome.health_label, "healthy");
        assert_eq!(
            envelope.view_model.session.project_path.as_deref(),
            Some("/tmp/project")
        );
    }

    #[test]
    fn stale_file_read_result_requires_current_root_and_path() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        let old = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::ReadFile {
                    root: "project_ai".to_string(),
                    path: "README.md".to_string(),
                },
            },
        });
        let new = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::ReadFile {
                    root: "user_ai".to_string(),
                    path: "README.md".to_string(),
                },
            },
        });

        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: new[0].id,
                ok: true,
                kind: RyeOsEffectResultKind::FileRead,
                data: Some(serde_json::json!({
                    "root": "user_ai",
                    "path": "README.md",
                    "content": "new"
                })),
                error: None,
            },
        });
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: old[0].id,
                ok: true,
                kind: RyeOsEffectResultKind::FileRead,
                data: Some(serde_json::json!({
                    "root": "project_ai",
                    "path": "README.md",
                    "content": "old"
                })),
                error: None,
            },
        });

        assert_eq!(
            core.data
                .file_read
                .as_ref()
                .map(|file| file.content.as_str()),
            Some("new")
        );
    }

    #[test]
    fn mismatched_effect_result_does_not_apply_data() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:ryeos/items/space");
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        let fetch_items = effects
            .iter()
            .find(|effect| matches!(effect.kind, RyeOsEffectKind::FetchSource { .. }))
            .expect("open bound view should fetch its source");

        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: fetch_items.id,
                ok: true,
                kind: RyeOsEffectResultKind::Dimension,
                data: Some(serde_json::json!({
                    "schema_version": "ryeos.test",
                    "session": { "session_id": "session-1", "surface_ref": "surface:ryeos/ryeos/base", "read_only": true },
                    "local_node": { "health": { "status": "healthy" }, "services": [] }
                })),
                error: None,
            },
        });

        assert!(core.data.dimension.is_none());
        assert!(core.data.items.is_none());
        assert_eq!(core.ui.notices.len(), 1);
    }

    #[test]
    fn thread_tail_deltas_accumulate_then_clear_on_durable() {
        let mut core = RyeOsCore::default();

        // Streaming cognition deltas accumulate; no refetch while live.
        let effects = core.dispatch(RyeOsEvent::ThreadTail {
            thread_id: "T-1".to_string(),
            event_type: "cognition_out".to_string(),
            payload: serde_json::json!({ "delta": "Hel" }),
        });
        assert!(effects.is_empty());
        core.dispatch(RyeOsEvent::ThreadTail {
            thread_id: "T-1".to_string(),
            event_type: "cognition_out".to_string(),
            payload: serde_json::json!({ "delta": "lo" }),
        });
        assert_eq!(
            core.data.live_delta.as_ref().map(|d| d.text.as_str()),
            Some("Hello")
        );

        // The settled turn (content, no delta) supersedes the live buffer.
        core.dispatch(RyeOsEvent::ThreadTail {
            thread_id: "T-1".to_string(),
            event_type: "cognition_out".to_string(),
            payload: serde_json::json!({ "content": "Hello", "turn": 1 }),
        });
        assert!(core.data.live_delta.is_none());
    }

    #[test]
    fn thread_tail_ephemeral_nontext_is_noop() {
        let mut core = RyeOsCore::default();
        let effects = core.dispatch(RyeOsEvent::ThreadTail {
            thread_id: "T-1".to_string(),
            event_type: "stream_opened".to_string(),
            payload: serde_json::json!({}),
        });
        assert!(effects.is_empty());
        assert!(core.data.live_delta.is_none());
    }

    // --- Dedicated coverage for the extracted ratchet path (`try_ratchet_route`
    // and the `apply_launch_outcome` delivery tower). ---

    /// Deliver a launched `Invoked` outcome after seeding a targeting route,
    /// returning the resulting seat route. `mutate` runs between submit and
    /// delivery (e.g. to move the route so the result is stale).
    fn launch_and_deliver(
        core: &mut RyeOsCore,
        text: &str,
        data: serde_json::Value,
        mutate: impl FnOnce(&mut RyeOsCore),
    ) {
        seed_service_route(core);
        set_focused_input(core, text);
        let effect = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        mutate(core);
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::Invoked,
                data: Some(data),
                error: None,
            },
        });
    }

    #[test]
    fn ratchet_route_seq_mismatch_warns_and_leaves_route_unchanged() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "first",
            serde_json::json!({ "thread_id": "T-stale", "delivery": "launched" }),
            |core| {
                // Route moves after the submit was issued → the result is stale.
                core.seat.append_facet(
                    crate::ui::seat::KEY_INPUT_ROUTE,
                    serde_json::json!({
                        "invoke": { "type": "service", "ref": "service:threads/input" },
                        "thread": "T-other"
                    }),
                );
            },
        );
        let route = core.seat.fold().input_route();
        assert_eq!(
            route.thread.as_deref(),
            Some("T-other"),
            "stale result must not retarget"
        );
        assert!(
            core.ui
                .notices
                .iter()
                .any(|n| n.message.contains("Route changed since submit")
                    && n.tone == RyeOsTone::Warn),
            "a stale ratchet surfaces the route-changed warning"
        );
    }

    #[test]
    fn ratchet_skips_retarget_when_thread_refuses_operator_followup() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "run graph",
            // Machine-continuing graph: continuation-capable but takes no operator input.
            serde_json::json!({
                "thread_id": "G-1",
                "delivery": "launched",
                "execution": { "supports_continuation": true, "supports_operator_followup": false }
            }),
            |_| {},
        );
        let route = core.seat.fold().input_route();
        assert_eq!(
            route.thread, None,
            "no operator follow-up → route not retargeted"
        );
        assert_eq!(route.chain_root, None);
    }

    #[test]
    fn ratchet_first_turn_sets_chain_root_and_head() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "hello",
            serde_json::json!({ "thread_id": "T-1", "delivery": "launched" }),
            |_| {},
        );
        let route = core.seat.fold().input_route();
        // First turn: the launched thread is both the chain root and the head.
        assert_eq!(route.chain_root.as_deref(), Some("T-1"));
        assert_eq!(route.thread.as_deref(), Some("T-1"));
    }

    #[test]
    fn ratchet_continuation_keeps_root_and_advances_head() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "hello",
            serde_json::json!({ "thread_id": "T-1", "delivery": "launched" }),
            |_| {},
        );
        // Second turn braids onto T-1: new head, same root.
        set_focused_input(&mut core, "again");
        let effect = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::Invoked,
                data: Some(serde_json::json!({ "thread_id": "T-2", "delivery": "launched" })),
                error: None,
            },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(
            route.thread.as_deref(),
            Some("T-2"),
            "head advances to the newest turn"
        );
        assert_eq!(
            route.chain_root.as_deref(),
            Some("T-1"),
            "root is preserved across the braid"
        );
    }

    #[test]
    fn ratchet_refused_delivery_preserves_buffer_and_route() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "hold this",
            serde_json::json!({
                "thread_id": serde_json::Value::Null,
                "delivery": "refused",
                "notice": "thread is not continuation-capable"
            }),
            |_| {},
        );
        assert_eq!(
            focused_input_text(&core),
            "hold this",
            "refused delivery keeps the buffer"
        );
        let route = core.seat.fold().input_route();
        assert_eq!(route.thread, None, "refused → no ratchet");
        assert_eq!(route.chain_root, None);
    }

    #[test]
    fn ratchet_submitted_delivery_clears_buffer_without_retarget_and_warns_when_degraded() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "steer it",
            serde_json::json!({
                "thread_id": "T-live",
                "delivery": "submitted",
                "notice": "interrupt degraded to steer"
            }),
            |_| {},
        );
        // Live fold into a running thread: buffer clears, but no new thread and no ratchet.
        assert_eq!(focused_input_text(&core), "");
        let route = core.seat.fold().input_route();
        assert_eq!(route.thread, None, "submitted → no ratchet");
        assert!(
            core.ui
                .notices
                .iter()
                .any(|n| n.message.contains("interrupt degraded to steer")
                    && n.tone == RyeOsTone::Warn),
            "a degraded submitted delivery surfaces its notice as a warning"
        );
    }

    #[test]
    fn submitted_pending_delivery_coalesces_queue_notice() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "first steer",
            serde_json::json!({
                "thread_id": "T-live",
                "delivery": "submitted",
                "notice": "Input queued (1 staged).",
                "pending": 1
            }),
            |_| {},
        );
        launch_and_deliver(
            &mut core,
            "second steer",
            serde_json::json!({
                "thread_id": "T-live",
                "delivery": "submitted",
                "notice": "Input queued (2 staged).",
                "pending": 2
            }),
            |_| {},
        );

        let queue_notices: Vec<_> = core
            .ui
            .notices
            .iter()
            .filter(|notice| notice.message.starts_with(INPUT_QUEUE_NOTICE_PREFIX))
            .collect();
        assert_eq!(queue_notices.len(), 1);
        assert_eq!(
            queue_notices[0].message,
            "Queued behind active thread · 2 staged inputs."
        );
        assert_eq!(queue_notices[0].tone, RyeOsTone::Accent);
    }
}
