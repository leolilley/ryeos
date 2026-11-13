//! Seat I/O the runtime starts on the host's behalf: the live history
//! reader, hint and tail streams, the append writer, presence renewal and
//! same-producer reproof.

use super::*;

impl<P: Platform + 'static> ClientRuntime<P> {
    // -----------------------------------------------------------------
    // Seat I/O
    // -----------------------------------------------------------------

    fn is_active_producer(&self, producer: &UiPresentationProducer) -> bool {
        self.seat
            .as_ref()
            .is_some_and(|seat| seat.producer == *producer)
    }

    pub(super) fn fail(&mut self, reason: StopReason) {
        self.shared.delivery.borrow_mut().blocked = true;
        if self.pending_stop.is_none() {
            self.pending_stop = Some(reason);
        }
        self.inbox.push(StreamTag::default(), InboxPayload::Wake);
    }

    pub(super) fn spawn_live(
        &mut self,
        history: LiveHistory<Frames<P>>,
        producer: UiPresentationProducer,
    ) {
        let shared = self.shared.clone();
        let inbox = self.inbox.clone();
        let tag = self.seat_tag.clone();
        let task = self.shared.platform.spawner().spawn(async move {
            let mut history = history;
            loop {
                let event = history.next().await;
                if !shared.is_current(&producer) {
                    return;
                }
                let ended = match event {
                    LiveEvent::Verified(append) => {
                        if shared.delivery.borrow_mut().accept_verified(&append) {
                            continue;
                        }
                        shared.delivery.borrow_mut().blocked = true;
                        LiveEvent::Invalidated(
                            "verified seat append is not this producer's exact pending or acknowledged operation"
                                .to_owned(),
                        )
                    }
                    LiveEvent::Invalidated(reason) => {
                        shared.delivery.borrow_mut().blocked = true;
                        LiveEvent::Invalidated(reason)
                    }
                    LiveEvent::ReproofRequired => LiveEvent::ReproofRequired,
                };
                inbox.push(
                    tag.clone(),
                    InboxPayload::Task(TaskResult::new(SeatTask::<P>::LiveEnded {
                        producer: producer.clone(),
                        event: ended,
                    })),
                );
                return;
            }
        });
        self.live_task = Some(task);
    }

    pub(super) fn restart_hints(&mut self, core: &RyeOsCore) {
        self.hints_task = None;
        if self.closing {
            return;
        }
        if let Some(path) = hints_path(core) {
            self.hints_task = Some(spawn_hints(
                &self.shared.platform,
                &self.inbox,
                self.hints_tag.clone(),
                path,
            ));
        }
    }

    /// Follow the focused route: one tail per chain, reopened when the chain
    /// changes.
    pub(super) fn sync_tail(&mut self, core: &RyeOsCore) {
        if self.closing {
            return;
        }
        let route = core.focused_input_route();
        let desired = route.chain_root.or(route.thread);
        if desired == self.tail_chain {
            return;
        }
        self.tail_task = None;
        self.tail_chain = desired.clone();
        self.tail_tag = self.stamp();
        self.inbox
            .set_current(Stream::FocusedTail, self.tail_tag.clone());
        let Some(chain) = desired else {
            return;
        };
        if let Ok(path) = chain_events_stream_path(&chain) {
            self.tail_task = Some(spawn_tail(
                &self.shared.platform,
                &self.inbox,
                self.tail_tag.clone(),
                path,
                chain,
            ));
        }
    }

    /// Hand the writer the next unmirrored slice, at most one append in
    /// flight. A failed append fences this writer; it never creates
    /// replacement authority or marks an unknown append acknowledged.
    pub(super) fn queue_seat_sync(&mut self, core: &RyeOsCore) {
        if self.append_inflight || self.touch_inflight {
            return;
        }
        let Some(producer) = self.shared.current_producer() else {
            return;
        };
        // Only the active seat's own producer writes: while a replacement is
        // being folded the shared producer is already the successor's, and
        // the predecessor core's events must not be appended under it.
        if !self.is_active_producer(&producer) {
            return;
        }
        let (blocked, first, limit) = {
            let delivery = self.shared.delivery.borrow();
            (
                delivery.blocked,
                delivery.next_engine_seq,
                delivery.max_seat_append_bytes,
            )
        };
        if blocked {
            return;
        }
        let events = core.seat.events();
        let acknowledged = core.acknowledged_seat_prefix_len();
        if events.len() <= acknowledged {
            return;
        }
        let plain: Result<Vec<PlainSeatEvent>, _> = events[acknowledged..]
            .iter()
            .take(MAX_SEAT_BATCH_EVENTS)
            .map(PlainSeatEvent::try_from)
            .collect();
        let count = plain.map_err(|error| error.0).and_then(|plain| {
            plan_append_len(&plain, &producer, first, limit).map_err(|error| error.0)
        });
        let count = match count {
            Ok(count) => count,
            Err(message) => {
                self.fail(StopReason::Fatal(message));
                return;
            }
        };
        let captured = events[acknowledged..acknowledged + count].to_vec();
        let upto = acknowledged + count;
        let shared = self.shared.clone();
        let inbox = self.inbox.clone();
        let tag = self.seat_tag.clone();
        let retry = self.retry;
        self.append_inflight = true;
        let task = self.shared.platform.spawner().spawn(async move {
            let result = send_append(&shared, &producer, &captured, retry).await;
            inbox.push(
                tag,
                InboxPayload::Task(TaskResult::new(SeatTask::<P>::Append {
                    producer,
                    events: captured,
                    upto,
                    result,
                })),
            );
        });
        self.io_tasks.push(task);
    }

    pub(super) fn maybe_touch(&mut self) {
        let now = self.now_ms();
        if now.saturating_sub(self.last_touch_ms) < TOUCH_INTERVAL_MS {
            return;
        }
        let (pending, blocked) = {
            let delivery = self.shared.delivery.borrow();
            (delivery.pending.is_some(), delivery.blocked)
        };
        if !may_start_seat_touch(self.append_inflight, self.touch_inflight, pending, blocked) {
            return;
        }
        let Some(producer) = self.shared.current_producer() else {
            return;
        };
        if !self.is_active_producer(&producer) {
            return;
        }
        self.last_touch_ms = now;
        self.touch_inflight = true;
        let shared = self.shared.clone();
        let inbox = self.inbox.clone();
        let tag = self.seat_tag.clone();
        let task = self.shared.platform.spawner().spawn(async move {
            let outcome = touch_seat(&shared, &producer).await;
            inbox.push(
                tag,
                InboxPayload::Task(TaskResult::new(SeatTask::<P>::Touch { producer, outcome })),
            );
        });
        self.io_tasks.push(task);
    }

    pub(super) fn maybe_start_reproof(&mut self, core: &RyeOsCore) {
        if !self.reproof_pending || self.reproof_task.is_some() || self.closing {
            return;
        }
        let Some(seat) = &self.seat else {
            return;
        };
        let producer = seat.producer.clone();
        let (claim, claim_next) = (seat.claim_request.clone(), seat.claim_next_engine_seq);
        let settled = {
            let delivery = self.shared.delivery.borrow();
            !delivery.blocked && delivery.pending.is_none()
        };
        if self.append_inflight
            || !settled
            || !self.shared.is_current(&producer)
            || core.seat.events().len() != core.acknowledged_seat_prefix_len()
        {
            return;
        }
        let guard = match core.begin_authenticated_reproof() {
            Ok(guard) => guard,
            Err(message) => {
                self.fail(StopReason::Fatal(message));
                return;
            }
        };
        let Some(session) = core.data.session.clone() else {
            self.fail(StopReason::Fatal("reproof session absent".to_owned()));
            return;
        };
        let viewport = core.runtime.viewport;
        let config = self.config;
        let generation = {
            self.generation += 1;
            self.generation
        };
        self.reproof_generation = Some(generation);
        let shared = self.shared.clone();
        let inbox = self.inbox.clone();
        let tag = self.seat_tag.clone();
        let task = self.shared.platform.spawner().spawn(async move {
            let result = reproof_seat(
                &shared, guard, &producer, &claim, claim_next, &session, viewport, config,
            )
            .await;
            inbox.push(
                tag,
                InboxPayload::Task(TaskResult::new(SeatTask::<P>::Reproof {
                    generation,
                    producer,
                    result,
                })),
            );
        });
        self.reproof_task = Some(task);
    }

    /// Call after every change the host made to the core (key, paste, resize,
    /// tick). It keeps the seat in step: follows the focused route's tail,
    /// accounts for new local seat events (blocking the seat if more than 512
    /// events or 512 KiB would pile up behind a stuck writer), mirrors them
    /// to the daemon, renews presence on its cadence, and starts a
    /// same-producer reproof when one is due.
    pub fn after_mutation(&mut self, core: &RyeOsCore) {
        self.io_tasks.retain(|task| !task.is_finished());
        if self.seat.is_none() {
            return;
        }
        self.viewport.set(core.runtime.viewport);
        self.sync_tail(core);
        let acknowledged = core.acknowledged_seat_prefix_len();
        if let Err(message) = self.tracker.observe(core.seat.events(), acknowledged) {
            self.fail(StopReason::Fatal(message));
            return;
        }
        self.maybe_start_reproof(core);
        if !self.closing {
            self.maybe_touch();
        }
        self.queue_seat_sync(core);
        self.persist_layout(core);
    }

    /// Save the arrangement after an accepted edit, debounced.
    fn persist_layout(&mut self, core: &RyeOsCore) {
        let now = self.now_ms();
        let store = self.shared.platform.store();
        self.layout.observe(
            now,
            core.layout_preference_key().ok(),
            || core.export_layout_preferences(),
            store,
        );
        self.layout.write_due(now, store);
    }

    /// Load the cached arrangement for the core's current scope and apply it.
    /// Nothing stored, or no scope yet, restores nothing.
    pub fn restore_layout_preferences(
        &mut self,
        core: &mut RyeOsCore,
    ) -> Result<Vec<RyeOsEffect>, RuntimeError> {
        let Ok(key) = core.layout_preference_key() else {
            return Ok(Vec::new());
        };
        let Some(saved) = self.shared.platform.store().get(&key) else {
            return Ok(Vec::new());
        };
        core.restore_layout_preferences(&saved)
            .map_err(RuntimeError::Fatal)
    }

    /// Layout-preference errors (a failed export or write) since the last
    /// call. A failed write stays pending and is retried at the next flush.
    pub fn take_layout_errors(&mut self) -> Vec<String> {
        self.layout.take_errors()
    }
}
