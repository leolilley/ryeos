//! Folding what asynchronous work produced into the core, in order.

use super::*;

impl<P: Platform + 'static> ClientRuntime<P> {
    // -----------------------------------------------------------------
    // Drain
    // -----------------------------------------------------------------

    /// Fold everything asynchronous work produced, in order, into the core.
    pub fn drain(&mut self, core: &mut RyeOsCore) -> Drained {
        let mut out = Drained::default();
        // Chosen behaviour: effects emitted before a turnover in this drain are
        // still submitted, under the successor's tag; the core ignores their results.
        self.reap_batches();
        for message in self.inbox.drain() {
            // A fold earlier in this batch can turn the streams over; what it
            // left behind belongs to the predecessor.
            if !self.inbox.accepts(message.payload.stream(), &message.tag) {
                continue;
            }
            match message.payload {
                InboxPayload::EffectResults(results) => {
                    self.fold_effect_results(core, results, &mut out);
                }
                InboxPayload::Hint(payload) => {
                    let kind = payload
                        .get("kind")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    if let Some(kind) = kind {
                        let effects = core.dispatch(RyeOsEvent::HintReceived { kind, payload });
                        out.effects.extend(effects);
                        out.changed = true;
                    }
                }
                InboxPayload::HintFlush(kinds) => {
                    let effects = core.dispatch(RyeOsEvent::HintFlushBatch { kinds });
                    out.effects.extend(effects);
                    out.changed = true;
                }
                InboxPayload::DaemonEvent(payload) => {
                    let effects = core.dispatch(RyeOsEvent::DaemonEvent { payload });
                    out.effects.extend(effects);
                    out.changed = true;
                }
                InboxPayload::Resubscribed => {
                    // The core does not refetch on its own after a gap.
                    out.effects.extend(core.initial_effects());
                    out.changed = true;
                }
                InboxPayload::TransportState {
                    channel,
                    freshness,
                    observed_at_ms,
                } => {
                    let effects = core.dispatch(RyeOsEvent::TransportStateChanged {
                        channel,
                        freshness,
                        observed_at_ms: Some(observed_at_ms),
                        error: None,
                    });
                    out.effects.extend(effects);
                    out.changed = true;
                }
                InboxPayload::ThreadTail {
                    chain_root_id,
                    frame,
                } => {
                    let route = core.focused_input_route();
                    let chain = route.chain_root.as_deref().or(route.thread.as_deref());
                    let thread = route.thread.as_deref().or(chain);
                    if chain != Some(chain_root_id.as_str()) {
                        continue;
                    }
                    if let Some((event_type, payload)) = focused_tail_payload(&frame, chain, thread)
                    {
                        let thread_id = thread.unwrap_or_default().to_owned();
                        let effects = core.dispatch(RyeOsEvent::ThreadTail {
                            thread_id,
                            event_type,
                            payload,
                        });
                        out.effects.extend(effects);
                        out.changed = true;
                    }
                }
                InboxPayload::Task(task) => self.fold_task(core, &task, &mut out),
                InboxPayload::Wake => out.changed = true,
                InboxPayload::SeatAck(_) | InboxPayload::ProducerReady(_) => {}
            }
        }
        self.after_mutation(&*core);
        if out.stop.is_none() {
            out.stop = self.pending_stop.take();
        } else {
            self.pending_stop = None;
        }
        out.seat = self.seat_status();
        out
    }

    pub(super) fn fold_effect_results(
        &mut self,
        core: &mut RyeOsCore,
        results: Vec<RyeOsEffectResult>,
        out: &mut Drained,
    ) {
        let previous = session_id_of(core);
        let mut follow: Vec<RyeOsEffect> = Vec::new();
        let mut replaced = false;
        for result in results {
            if replaced {
                // The rest of the batch belonged to the predecessor; the
                // successor core has no pending coordinate for it.
                continue;
            }
            let before = session_id_of(core);
            let emitted = core.dispatch(RyeOsEvent::EffectResult { result });
            if session_id_of(core) != before {
                // Earlier follow-ups derived from the predecessor are
                // discarded; only successor bootstrap work crosses the cut.
                follow = emitted;
                replaced = true;
            } else {
                follow.extend(emitted);
            }
        }
        out.changed = true;
        if session_id_of(core) != previous {
            out.turned_over = true;
            self.turn_over(core, &mut follow, out);
        } else if let Some(handoff) = self.shared.take_handoff() {
            // An effect produced a claimed stream the reducer did not adopt:
            // never leave that producer live without an observing core.
            self.shared.fence(&handoff.producer);
            out.stop_with(StopReason::Fatal(
                "replacement live history was not admitted by the core".to_owned(),
            ));
        }
        out.effects.extend(follow);
    }

    /// A project or surface authority change created a new immutable UI
    /// session. Every transport keyed by the predecessor turns over with it:
    /// hints, tail and the live history reader.
    pub(super) fn turn_over(
        &mut self,
        core: &mut RyeOsCore,
        follow: &mut Vec<RyeOsEffect>,
        out: &mut Drained,
    ) {
        self.hints_task = None;
        self.tail_task = None;
        self.tail_chain = None;
        self.live_task = None;
        self.reproof_task = None;
        self.reproof_generation = None;
        self.reproof_pending = false;
        let Some(producer) = self.shared.current_producer() else {
            out.stop_with(StopReason::Fatal(
                "replacement producer is not ready".to_owned(),
            ));
            return;
        };
        let Some(handoff) = self.shared.take_handoff() else {
            self.shared.fence(&producer);
            out.stop_with(StopReason::Fatal(
                "replacement authenticated live history was not handed off".to_owned(),
            ));
            return;
        };
        if handoff.producer != producer {
            self.shared.fence(&producer);
            out.stop_with(StopReason::Fatal(
                "replacement live history belongs to another producer".to_owned(),
            ));
            return;
        }
        self.seat = Some(ActiveSeat {
            producer: producer.clone(),
            claim_request: handoff.claim_request.clone(),
            claim_next_engine_seq: handoff.claim_next_engine_seq,
        });
        self.append_inflight = false;
        self.touch_inflight = false;
        self.retag_streams();
        self.spawn_live(handoff.history, producer.clone());
        let next_engine_seq = self.shared.delivery.borrow().next_engine_seq;
        follow.extend(core.dispatch(RyeOsEvent::PresentationProducerReady {
            producer,
            next_engine_seq,
        }));
        self.tracker.reset(core.acknowledged_seat_prefix_len());
        self.last_touch_ms = self.now_ms();
        self.restart_hints(core);
        self.sync_tail(core);
    }

    pub(super) fn fold_task(&mut self, core: &mut RyeOsCore, task: &TaskResult, out: &mut Drained) {
        let Some(task) = task.take::<SeatTask<P>>() else {
            return;
        };
        match task {
            SeatTask::Append {
                producer,
                events,
                upto,
                result,
            } => {
                self.append_inflight = false;
                if !self.shared.is_current(&producer) {
                    return;
                }
                match result {
                    Ok(ack) => match replay_acknowledged_events(core, &producer, events, &ack.rows)
                    {
                        Ok(effects) => {
                            out.effects.extend(effects);
                            out.changed = true;
                            if core.acknowledged_seat_prefix_len() != upto {
                                self.shared.fence(&producer);
                                out.stop_with(StopReason::Fatal(
                                    "acknowledged prefix differs from the written prefix"
                                        .to_owned(),
                                ));
                            }
                        }
                        Err(message) => {
                            self.shared.fence(&producer);
                            out.stop_with(StopReason::Fatal(message));
                        }
                    },
                    Err(AppendFailure::Unknown(message)) => {
                        self.shared.fence(&producer);
                        out.stop_with(StopReason::Unavailable(Unavailable::AppendUnknown(message)));
                    }
                    Err(AppendFailure::Refused(message)) => {
                        self.shared.fence(&producer);
                        out.stop_with(StopReason::Unavailable(Unavailable::WriterRefused(message)));
                    }
                    Err(AppendFailure::Fatal(message)) => {
                        self.shared.fence(&producer);
                        out.stop_with(StopReason::Fatal(message));
                    }
                }
            }
            SeatTask::Touch { producer, outcome } => {
                self.touch_inflight = false;
                if !self.shared.is_current(&producer) {
                    return;
                }
                match outcome {
                    TouchOutcome::Renewed => {}
                    TouchOutcome::Retry => {
                        // Unconfirmed is not fenced; retry soon.
                        self.last_touch_ms = self
                            .now_ms()
                            .saturating_sub(TOUCH_INTERVAL_MS - TOUCH_RETRY_AFTER_MS);
                    }
                    TouchOutcome::Refused => {
                        self.shared.fence(&producer);
                        out.stop_with(StopReason::Unavailable(Unavailable::WriterRefused(
                            "seat presence was refused".to_owned(),
                        )));
                    }
                    TouchOutcome::SessionEnded => {
                        self.shared.fence(&producer);
                        out.stop_with(StopReason::Unavailable(Unavailable::WriterRefused(
                            "The browser session ended.".to_owned(),
                        )));
                    }
                }
            }
            SeatTask::LiveEnded { producer, event } => {
                if !self.shared.is_current(&producer) {
                    return;
                }
                match event {
                    LiveEvent::ReproofRequired => self.reproof_pending = true,
                    LiveEvent::Invalidated(reason) => {
                        self.shared.fence(&producer);
                        out.stop_with(StopReason::Unavailable(Unavailable::WriterRefused(reason)));
                    }
                    LiveEvent::Verified(_) => {}
                }
            }
            SeatTask::Reproof {
                generation,
                producer,
                result,
            } => {
                let current = self.reproof_generation == Some(generation)
                    && self.shared.is_current(&producer);
                self.reproof_task = None;
                if !current {
                    // An obsolete reply neither promotes nor fails its
                    // successor.
                    if self.reproof_generation == Some(generation) {
                        self.reproof_generation = None;
                    }
                    return;
                }
                self.reproof_generation = None;
                match result {
                    Ok(output) => {
                        let ReproofOutput {
                            candidate,
                            completion,
                            history,
                        } = output;
                        match candidate.commit(&completion, Some(&mut *core)) {
                            Ok(ReplayCommit::Promoted { effects }) => {
                                self.tracker.reset(core.acknowledged_seat_prefix_len());
                                self.spawn_live(history, producer);
                                self.reproof_pending = false;
                                out.effects.extend(effects);
                                out.changed = true;
                            }
                            Ok(ReplayCommit::Cold { .. }) => {
                                self.shared.fence(&producer);
                                out.stop_with(StopReason::Fatal(
                                    "reproof produced a new core instead of a promotion".to_owned(),
                                ));
                            }
                            Err(message) => {
                                self.shared.fence(&producer);
                                out.stop_with(StopReason::Fatal(message));
                            }
                        }
                    }
                    Err(error) => {
                        self.shared.fence(&producer);
                        out.stop_with(StopReason::Unavailable(Unavailable::WriterRefused(
                            error.to_string(),
                        )));
                    }
                }
            }
        }
    }
}
