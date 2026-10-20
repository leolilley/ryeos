//! The terminal seat runtime: one event loop folding terminal input,
//! braid tails, session hints, and ticks into the shared `RyeOsCore`.
//!
//! This module is the loop and nothing else. Effect execution lives in
//! `effects`, seat-thread lifecycle in `seat`, live-signal subscriptions
//! in `hints`, key translation in `keys`. Rendering is `crate::render`.

mod effects;
mod hints;
mod keys;
pub(crate) mod seat;

use std::collections::HashSet;
use std::sync::Arc;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use ryeos_client_base::ui::scene_model::SCENE_FRAME_MS;
use ryeos_client_base::ui::{BrowserViewport, RyeOsCore, RyeOsEffectResult, RyeOsEvent};

use crate::render::RyeOsTerminalRenderer;
use crate::terminal::TerminalGuard;
use crate::transport::daemon::DaemonClient;

use effects::spawn_effects;
use keys::{handle_key, should_quit};

enum SeatIoAck {
    Append {
        producer: ryeos_client_base::ui::binding::UiPresentationProducer,
        events: Vec<ryeos_client_base::ui::SeatEvent>,
        outcome: seat::SeatAppendOutcome,
        canonical: Vec<ryeos_client_base::ui::seat::RetainedSeatEvent>,
        upto: usize,
    },
    Touch {
        producer: ryeos_client_base::ui::binding::UiPresentationProducer,
        outcome: seat::SeatTouchOutcome,
    },
}

fn may_start_seat_touch(
    append_inflight: bool,
    touch_inflight: bool,
    pending_append: bool,
    delivery_blocked: bool,
) -> bool {
    !append_inflight && !touch_inflight && !pending_append && !delivery_blocked
}

/// A clean native close may not overtake a local event that has not reached
/// the exact acknowledged prefix. The event loop performs the bounded drain;
/// this predicate is only its final local close fence.
fn may_close_mirrored_seat(
    local_events: usize,
    acknowledged_events: usize,
    append_inflight: bool,
    touch_inflight: bool,
    effects_pending: bool,
) -> bool {
    local_events == acknowledged_events && !append_inflight && !touch_inflight && !effects_pending
}

fn root_seat_binding(
    session: &ryeos_client_base::ui::BrowserSession,
) -> Result<seat::SeatBindingCoordinate, std::io::Error> {
    let mut matches = session
        .binding_attachments
        .iter()
        .filter(|attachment| attachment.binding_attachment_id == session.root_attachment_id);
    let attachment = matches
        .next()
        .ok_or_else(|| std::io::Error::other("UI session omitted its authored root attachment"))?;
    if matches.next().is_some() {
        return Err(std::io::Error::other(
            "UI session duplicated its authored root attachment",
        ));
    }
    Ok(seat::SeatBindingCoordinate::from(attachment))
}

fn retained_seat_binding(
    session: &ryeos_client_base::ui::BrowserSession,
    presentation_id: &str,
) -> Result<seat::SeatBindingCoordinate, std::io::Error> {
    let mut memberships = session
        .presentations
        .iter()
        .filter(|membership| membership.presentation_id == presentation_id);
    let membership = memberships.next().ok_or_else(|| {
        std::io::Error::other("UI session has no retained presentation membership")
    })?;
    if memberships.next().is_some() {
        return Err(std::io::Error::other(
            "UI session duplicated the retained presentation membership",
        ));
    }
    let mut attachments = session.binding_attachments.iter().filter(|attachment| {
        attachment.binding_attachment_id == membership.root_binding.binding_attachment_id
            && attachment.binding_generation == membership.root_binding.binding_generation
            && attachment.binding_digest == membership.root_binding.binding_digest
    });
    let attachment = attachments.next().ok_or_else(|| {
        std::io::Error::other("retained presentation root binding is not a current attachment")
    })?;
    if attachments.next().is_some() {
        return Err(std::io::Error::other(
            "UI session duplicated the retained presentation root attachment",
        ));
    }
    Ok(seat::SeatBindingCoordinate::from(attachment))
}

async fn reject_postclaim_admission(
    client: &DaemonClient,
    producer: &ryeos_client_base::ui::binding::UiPresentationProducer,
    resumed: bool,
) {
    if resumed {
        // A completed retained replay can have generated local work while
        // rebuilding the candidate. Do not declare its owner relinquished
        // merely because a later session observation could not be compared.
        let _ = seat::fence_failed_bootstrap(client, producer).await;
    } else {
        seat::relinquish_seat_thread(client, &producer.thread_id).await;
    }
}

pub async fn run(
    diagnostics: Vec<String>,
    client: DaemonClient,
    resume_presentation_id: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Entry admission and every effect retain the same verified daemon client.
    let mut term = Some(TerminalGuard::init()?);
    let (width, height) = term.as_ref().expect("terminal initialized").size();
    let mut stdout = std::io::stdout();
    let mut renderer = RyeOsTerminalRenderer::new();
    let mut dirty = true;

    // Degraded live timeline: tail the route's head thread over SSE.
    // The braid is the truth; this is it arriving now. Retargets abort
    // the old tail and open a new one.
    let (tail_tx, mut tail_rx) = tokio::sync::mpsc::unbounded_channel::<(String, String, String)>();
    let mut tail_chain: Option<String> = None;
    let mut tail_task: Option<tokio::task::JoinHandle<()>> = None;

    let current_session = client.current_ui_session().await?;
    let seat_binding = match resume_presentation_id.as_deref() {
        Some(presentation_id) => retained_seat_binding(&current_session, presentation_id)?,
        None => root_seat_binding(&current_session)?,
    };
    // Both paths claim through the same daemon seat owner before permitting
    // Rust effects. Resume proves a complete retained prefix in a private
    // candidate core; Fresh creates the core after its singleton proof.
    let (initial_bootstrap, resumed_core) = match resume_presentation_id.as_deref() {
        Some(presentation_id) => {
            let (bootstrap, core) = seat::resume_seat_thread(
                &client,
                &seat_binding,
                presentation_id,
                &current_session,
                viewport(width, height),
                now_ms(),
            )
            .await
            .map_err(std::io::Error::other)?;
            (bootstrap, Some(core))
        }
        None => (
            seat::bootstrap_seat(&client, &seat_binding)
                .await
                .map_err(std::io::Error::other)?,
            None,
        ),
    };
    let producer = initial_bootstrap.producer.clone();
    let resumed = resumed_core.is_some();
    let admitted_session = match client.current_ui_session().await {
        Ok(session) => session,
        Err(error) => {
            reject_postclaim_admission(&client, &producer, resumed).await;
            return Err(error.into());
        }
    };
    if admitted_session.session_id != current_session.session_id {
        reject_postclaim_admission(&client, &producer, resumed).await;
        return Err(std::io::Error::other("session changed during terminal admission").into());
    }
    let memberships = admitted_session
        .presentations
        .iter()
        .filter(|membership| membership.presentation_id == producer.presentation_id)
        .collect::<Vec<_>>();
    if memberships.len() != 1
        || memberships[0].thread_id.as_deref() != Some(producer.thread_id.as_str())
        || memberships[0].chain_root_id.as_deref() != Some(initial_bootstrap.chain_root_id.as_str())
        || memberships[0].root_binding.binding_attachment_id != seat_binding.binding_attachment_id
        || memberships[0].root_binding.binding_generation != seat_binding.binding_generation
        || memberships[0].root_binding.binding_digest != seat_binding.binding_digest
    {
        reject_postclaim_admission(&client, &producer, resumed).await;
        return Err(std::io::Error::other(
            "terminal presentation membership changed before core construction",
        )
        .into());
    }
    let resumed_session_unchanged = resumed_core.as_ref().is_none_or(|core| {
        matches!(
            (
                serde_json::to_value(core.data.session.as_ref()),
                serde_json::to_value(&admitted_session),
            ),
            (Ok(prior), Ok(current)) if prior == current
        )
    });
    if !resumed_session_unchanged {
        reject_postclaim_admission(&client, &producer, resumed).await;
        return Err(std::io::Error::other(
            "authenticated UI session changed during retained seat replay",
        )
        .into());
    }
    let (mut core, replay_effects) = match resumed_core {
        Some(core) => (core, initial_bootstrap.replay_effects.clone()),
        None => {
            let mut core = RyeOsCore::new(
                admitted_session,
                &producer.presentation_id,
                viewport(width, height),
                now_ms(),
            );
            let replayed = initial_bootstrap
                .replayed
                .iter()
                .map(|row| row.event.clone())
                .collect::<Vec<_>>();
            if let Err(error) = core.validate_replayed_initialization(&replayed) {
                seat::relinquish_seat_thread(&client, &producer.thread_id).await;
                return Err(std::io::Error::other(error).into());
            }
            let effects = match core.replay_authenticated_seat_events(initial_bootstrap.replayed) {
                Ok(effects) => effects,
                Err(error) => {
                    seat::relinquish_seat_thread(&client, &producer.thread_id).await;
                    return Err(std::io::Error::other(error).into());
                }
            };
            (core, effects)
        }
    };
    if let Err(error) = core.require_replayed_initialization() {
        drop(term.take());
        // A replay fold can reject an acknowledged composition without
        // changing the event count. Neither count equality nor an empty
        // effect list proves this claimed seat is safe to close.
        eprintln!(
            "RyeOS retained replay failed; seat {} remains open because replay acceptance is uncertain",
            producer.thread_id
        );
        return Err(std::io::Error::other(error).into());
    }
    let initial_seat_synced = core.seat.events().len();
    let producer_effects = core.dispatch(RyeOsEvent::PresentationProducerReady {
        producer,
        next_engine_seq: initial_bootstrap.next_engine_seq,
    });
    if let Err(error) = core.require_presentation_producer_ready(
        &initial_bootstrap.producer,
        initial_bootstrap.next_engine_seq,
    ) {
        drop(term.take());
        eprintln!(
            "RyeOS producer readiness refused for seat {}; claimed owner remains lifecycle-owned",
            initial_bootstrap.thread_id
        );
        return Err(std::io::Error::other(error).into());
    }
    let start_effects = if resumed {
        let mut replay_and_producer_effects = replay_effects;
        replay_and_producer_effects.extend(producer_effects);
        core.initial_effects_after_cold_replay(replay_and_producer_effects)
    } else {
        let mut start_effects = producer_effects;
        start_effects.extend(replay_effects);
        start_effects.extend(core.initial_effects());
        start_effects
    };

    // Surface startup diagnostics (surface/view resolution warnings) as in-TUI
    // notices. Otherwise they print to stderr BEFORE the alternate screen is
    // entered, so they scroll off above the render where they can't be read.
    for message in diagnostics {
        core.notice(message, ryeos_client_base::ui::view_model::RyeOsTone::Warn);
    }

    // First frame before any daemon round trip: chrome, docks, and the
    // backdrop paint immediately; bound views fill in as sources land.
    {
        let vm = core.envelope(Vec::new()).view_model;
        if let Err(error) = renderer.render(&mut stdout, &vm, width, height) {
            // Producer-ready can finish a replayed prepared operation before
            // this first frame. No mirror writer exists yet, so closing that
            // seat would discard a local, unacknowledged event. Restore the
            // terminal before any cleanup request or diagnostic output.
            drop(term.take());
            if may_close_mirrored_seat(
                core.seat.events().len(),
                initial_seat_synced,
                false,
                false,
                !core.pending_effects.is_empty() || !start_effects.is_empty(),
            ) {
                seat::relinquish_seat_thread(&client, &initial_bootstrap.thread_id).await;
            } else {
                eprintln!(
                    "RyeOS first frame failed; seat {} remains open because local history or effects are unsettled",
                    initial_bootstrap.thread_id
                );
            }
            return Err(error.into());
        }
    }

    let client = Arc::new(client);

    // Keep reading the exact stream whose cold completion admitted this seat.
    // It buffers the handoff interval while the first frame is constructed.
    let mut live_claim = (
        initial_bootstrap.claim_request.clone(),
        initial_bootstrap.claim_next_engine_seq,
    );
    let mut seat_reproof_pending = false;
    let mut actor_seat_producer = initial_bootstrap.producer.clone();
    let (seat_live_tx, mut seat_live_rx) = tokio::sync::mpsc::unbounded_channel::<(
        ryeos_client_base::ui::binding::UiPresentationProducer,
        bool,
    )>();
    let mut seat_live_task = {
        let client = client.clone();
        let producer = initial_bootstrap.producer.clone();
        let stream = initial_bootstrap.live_history;
        let tip_seq = initial_bootstrap.replay_tip_seq;
        let tip_hash = initial_bootstrap.replay_tip_hash;
        let seat_live_tx = seat_live_tx.clone();
        tokio::spawn(async move {
            let reproof =
                seat::watch_live_history(&client, stream, &producer, tip_seq, tip_hash).await;
            let _ = seat_live_tx.send((producer, reproof));
        })
    };

    // Effect results come home over this channel: batches run off the
    // loop, folds happen on arrival, and a round trip in flight never
    // blocks a frame.
    let (effect_tx, mut effect_rx) =
        tokio::sync::mpsc::unbounded_channel::<Vec<RyeOsEffectResult>>();
    spawn_effects(&client, start_effects, &effect_tx);

    // Fresh admission is complete before the event loop or any source effects.
    let mut seat_thread = Some(initial_bootstrap.thread_id);
    let mut seat_synced: usize = initial_seat_synced;
    // One braid writer, one batch in flight: mirrored order matches local
    // append order, and the loop never waits on the mirror.
    let (seat_write_tx, mut seat_write_rx) = tokio::sync::mpsc::unbounded_channel::<(
        ryeos_client_base::ui::binding::UiPresentationProducer,
        Vec<ryeos_client_base::ui::SeatEvent>,
        usize,
    )>();
    let (seat_ack_tx, mut seat_ack_rx) = tokio::sync::mpsc::unbounded_channel::<SeatIoAck>();
    let seat_touch_ack_tx = seat_ack_tx.clone();
    {
        let client = client.clone();
        tokio::spawn(async move {
            while let Some((producer, events, upto)) = seat_write_rx.recv().await {
                let mut canonical = Vec::new();
                let outcome = seat::append_braid(&client, &producer, &events, &mut canonical).await;
                if seat_ack_tx
                    .send(SeatIoAck::Append {
                        producer,
                        events,
                        outcome,
                        canonical,
                        upto,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    let mut seat_sync_inflight = false;
    let mut seat_touch_inflight = false;

    // Session hints: transient "look" signals; bound views declaring
    // `refresh.on_hint` refetch their sources. This replaces polling —
    // content decides its own liveness.
    let (mut session_tx, mut session_rx) =
        tokio::sync::mpsc::unbounded_channel::<hints::SessionMessage>();
    let mut session_task = hints::spawn_session_listener(client.clone(), session_tx.clone());

    let mut events = Some(EventStream::new());
    // Adaptive frame clock: the backdrop's breathe/sweep animation reads
    // fluid at the scene cadence, so the tick quickens while the center is
    // empty (the diff renderer repaints only the handful of changed
    // cells). With tiles up nothing animates per-tick, so 4fps keeps the
    // debounce and seat sync cadence without burning cycles.
    const TICK_BACKDROP_MS: u64 = SCENE_FRAME_MS;
    const TICK_TILES_MS: u64 = 250;
    const HINT_FLUSH_MS: u64 = 500;
    let mut tick_ms = TICK_TILES_MS;
    let mut tick = frame_clock(tick_ms);
    // A live-filter edit defers its list refetch to the tick (debounce), so
    // typing a filter never blocks on a per-keystroke daemon round-trip.
    let mut feed_dirty = false;
    let mut pending_hints: HashSet<String> = HashSet::new();
    let mut hint_batch_started_ms: Option<u64> = None;
    let mut last_seat_touch_ms = now_ms();
    let mut terminal_error: Option<Box<dyn std::error::Error>> = None;
    // Once input ends, restore the host terminal immediately. The existing
    // reducer/effect/seat-ACK loop then gets a bounded chance to settle its
    // exact local prefix before any terminal close is attempted.
    let mut shutdown_deadline: Option<tokio::time::Instant> = None;
    let (seat_reproof_tx, mut seat_reproof_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut seat_reproof_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut seat_reproof_generation: Option<uuid::Uuid> = None;

    loop {
        if seat_reproof_pending
            && seat_reproof_task.is_none()
            && shutdown_deadline.is_none()
            && !seat_sync_inflight
            && core.seat.events().len() == seat_synced
            && client
                .ui_producer
                .read()
                .expect("producer lock poisoned")
                .as_ref()
                == Some(&actor_seat_producer)
        {
            let producer = client
                .ui_producer
                .read()
                .expect("producer lock poisoned")
                .clone();
            let preparation = core.begin_authenticated_reproof().and_then(|guard| {
                let session = core.data.session.clone().ok_or("reproof session absent")?;
                let producer = producer.ok_or("reproof current producer absent")?;
                Ok((guard, session, producer))
            });
            let (guard, session, producer) = match preparation {
                Ok(value) => value,
                Err(error) => {
                    terminal_error = Some(std::io::Error::other(error).into());
                    break;
                }
            };
            let refresh_client = client.clone();
            let refresh_tx = seat_reproof_tx.clone();
            let claim = live_claim.clone();
            let viewport = core.runtime.viewport;
            let generation = uuid::Uuid::new_v4();
            seat_reproof_generation = Some(generation);
            seat_reproof_task = Some(tokio::spawn(async move {
                let result = seat::reproof_seat_thread(
                    &refresh_client,
                    &producer,
                    &claim.0,
                    claim.1,
                    &session,
                    viewport,
                    now_ms(),
                )
                .await
                .map(|(candidate, handoff)| (guard, candidate, handoff));
                let _ = refresh_tx.send((generation, producer, result));
            }));
        }
        if shutdown_deadline.is_some() {
            queue_seat_sync(
                &client,
                &core,
                &seat_thread,
                seat_synced,
                &mut seat_sync_inflight,
                seat_touch_inflight,
                &seat_write_tx,
            );
            if may_close_mirrored_seat(
                core.seat.events().len(),
                seat_synced,
                seat_sync_inflight,
                seat_touch_inflight,
                !core.pending_effects.is_empty(),
            ) {
                break;
            }
        }
        if dirty && let Some(active_term) = term.as_ref() {
            let vm = core.envelope(Vec::new()).view_model;
            let (width, height) = active_term.size();
            if let Err(error) = renderer.render(&mut stdout, &vm, width, height) {
                terminal_error = Some(error.into());
                break;
            }
            dirty = false;
        }

        let want_tick_ms = if core.wants_fast_ticks() {
            TICK_BACKDROP_MS
        } else {
            TICK_TILES_MS
        };
        if want_tick_ms != tick_ms {
            tick_ms = want_tick_ms;
            tick = frame_clock(tick_ms);
        }

        // Reconcile the SSE tail with the route facet. The timeline source is
        // scoped by `input.route.chain_root`, so tail the same braid live; keep
        // the moving route head separately as the live-buffer owner.
        let route = core.focused_input_route();
        let desired_chain = route.chain_root.clone().or_else(|| route.thread.clone());
        let desired_thread = route.thread.clone().or_else(|| desired_chain.clone());
        if shutdown_deadline.is_none() && desired_chain != tail_chain {
            if let Some(task) = tail_task.take() {
                task.abort();
            }
            tail_chain = desired_chain.clone();
            if let Some(chain_root_id) = desired_chain {
                tail_task = Some(hints::spawn_thread_tail(
                    client.clone(),
                    chain_root_id,
                    tail_tx.clone(),
                ));
            }
        }
        let tail_thread = desired_thread;

        tokio::select! {
            Some((generation, producer, result)) = seat_reproof_rx.recv(), if shutdown_deadline.is_none() => {
                // An obsolete reply must not take a successor task handle or
                // turn its valid producer into a terminal refresh failure.
                if seat_reproof_generation != Some(generation) { continue; }
                // Hold the exact producer read exclusion through synchronous
                // promotion; an effect executor cannot replace it mid-check.
                let promotion = {
                    let current_producer = client.ui_producer.read().expect("producer lock poisoned");
                    if reproof_reply_is_current(seat_reproof_generation, generation, current_producer.as_ref(), &producer) {
                        Some(result.and_then(|(guard, candidate, handoff)| {
                            let effects = core.promote_authenticated_reproof(candidate, &guard)?;
                            Ok((effects, handoff, producer))
                        }))
                    } else {
                        None
                    }
                };
                let Some(promotion) = promotion else {
                    if let Some(task) = seat_reproof_task.take() { task.abort(); }
                    seat_reproof_generation = None;
                    // The executor admitted a successor, but its actor result
                    // has not yet installed the matching Core/handoff. Keep
                    // local input suspended until that existing owner folds it.
                    continue;
                };
                seat_reproof_task.take();
                seat_reproof_generation = None;
                match promotion {
                    Ok((effects, handoff, producer)) => {
                        seat_synced = core.acknowledged_seat_prefix_len();
                        seat_live_task.abort();
                        let live_client = client.clone();
                        let live_tx = seat_live_tx.clone();
                        seat_live_task = tokio::spawn(async move {
                            let reproof = seat::watch_live_history(&live_client, handoff.stream, &producer, handoff.tip_seq, handoff.tip_hash).await;
                            let _ = live_tx.send((producer, reproof));
                        });
                        spawn_effects(&client, effects, &effect_tx);
                        seat_reproof_pending = false;
                        dirty = true;
                    },
                    Err(error) => {
                        client.seat_delivery.lock().await.blocked = true;
                        terminal_error = Some(std::io::Error::other(error).into());
                        break;
                    }
                }
            }
            maybe_event = async { events.as_mut().expect("input active").next().await }, if events.is_some() => {
                let Some(Ok(event)) = maybe_event else {
                    events.take();
                    term.take();
                    shutdown_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
                    continue;
                };
                match event {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if should_quit(&core, &key) {
                            events.take();
                            term.take();
                            shutdown_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
                            continue;
                        }
                        if seat_reproof_pending { continue; }
                        let before = core.focused_input_buffer().map(|b| b.text.clone());
                        let effects = handle_key(&mut core, key);
                        // A live-filter edit applies instantly but returns no
                        // effects — its list refetch is debounced to the tick so
                        // typing never blocks on a daemon round-trip per key.
                        if effects.is_empty()
                            && core.key_context().input_is_live_filter
                            && core.focused_input_buffer().map(|b| b.text.clone()) != before
                        {
                            feed_dirty = true;
                        }
                        spawn_effects(&client, effects, &effect_tx);
                        queue_seat_sync(
                            &client,
                            &core,
                            &seat_thread,
                            seat_synced,
                            &mut seat_sync_inflight,
                            seat_touch_inflight,
                            &seat_write_tx,
                        );
                        dirty = true;
                    }
                    Event::Paste(text) => {
                        if seat_reproof_pending { continue; }
                        let before = core.focused_input_buffer().map(|b| b.text.clone());
                        let effects = core.insert_focused_input_text(&text);
                        if effects.is_empty()
                            && core.key_context().input_is_live_filter
                            && core.focused_input_buffer().map(|b| b.text.clone()) != before
                        {
                            feed_dirty = true;
                        }
                        spawn_effects(&client, effects, &effect_tx);
                        queue_seat_sync(
                            &client, &core, &seat_thread, seat_synced,
                            &mut seat_sync_inflight, seat_touch_inflight, &seat_write_tx,
                        );
                        dirty = true;
                    }
                    Event::Resize(w, h) => {
                        let _ = term.as_mut().expect("terminal active").update_size();
                        let effects = core.dispatch(RyeOsEvent::Resize { viewport: viewport(w, h) });
                        spawn_effects(&client, effects, &effect_tx);
                        dirty = true;
                    }
                    _ => {}
                }
            }
            Some(results) = effect_rx.recv() => {
                // Fold one batch's results in emission order; any effects
                // the folds emit form the next spawned generation.
                let previous_session_id = core
                    .data
                    .session
                    .as_ref()
                    .map(|session| session.session_id.clone());
                let mut follow = Vec::new();
                let mut replaced_generation = false;
                for result in results {
                    if replaced_generation {
                        // Every remaining result belonged to the predecessor's
                        // concurrent batch. The replacement core deliberately
                        // has no pending coordinate for it.
                        continue;
                    }
                    let before = core
                        .data
                        .session
                        .as_ref()
                        .map(|session| session.session_id.clone());
                    let emitted = core.dispatch(RyeOsEvent::EffectResult { result });
                    let after = core
                        .data
                        .session
                        .as_ref()
                        .map(|session| session.session_id.clone());
                    if after != before {
                        // Discard any follow-up derived from earlier results in
                        // the predecessor batch. Only successor bootstrap work
                        // may cross the immutable session cut.
                        follow = emitted;
                        replaced_generation = true;
                    } else {
                        follow.extend(emitted);
                    }
                }
                let current_session = core.data.session.as_ref().map(|session| {
                    (session.session_id.clone(), root_seat_binding(session))
                });
                let current_session_id = current_session
                    .as_ref()
                    .map(|(session_id, _)| session_id.clone());
                if current_session_id != previous_session_id {
                    // A project/surface authority change creates a new immutable
                    // UI session. Every transport keyed by the predecessor must
                    // turn over with it; changing only the request cookie would
                    // leave hints and the durable seat attached to stale authority.
                    session_task.abort();
                    let (replacement_tx, replacement_rx) =
                        tokio::sync::mpsc::unbounded_channel::<hints::SessionMessage>();
                    session_tx = replacement_tx;
                    session_rx = replacement_rx;
                    session_task =
                        hints::spawn_session_listener(client.clone(), session_tx.clone());

                    if let Some(task) = tail_task.take() {
                        task.abort();
                    }
                    tail_chain = None;
                    pending_hints.clear();
                    hint_batch_started_ms = None;

                    // The effect owner has already admitted the successor. Do
                    // not clear that producer or allocate another presentation.
                    let Some(producer) = client.ui_producer.read().expect("producer lock poisoned").clone() else {
                        terminal_error = Some(std::io::Error::other("replacement producer is not ready").into());
                        break;
                    };
                    let Some(handoff) = client.seat_live_handoff.lock().await.take() else {
                        client.seat_delivery.lock().await.blocked = true;
                        *client.ui_producer.write().expect("producer lock poisoned") = None;
                        terminal_error = Some(std::io::Error::other("replacement authenticated live history was not handed off").into());
                        break;
                    };
                    if handoff.producer != producer {
                        client.seat_delivery.lock().await.blocked = true;
                        *client.ui_producer.write().expect("producer lock poisoned") = None;
                        terminal_error = Some(std::io::Error::other("replacement live history belongs to another producer").into());
                        break;
                    }
                    seat_live_task.abort();
                    live_claim = (handoff.claim_request.clone(), handoff.claim_next_engine_seq);
                    seat_reproof_generation = None;
                    if let Some(task) = seat_reproof_task.take() { task.abort(); }
                    seat_reproof_pending = false;
                    let live_client = client.clone();
                    let live_tx = seat_live_tx.clone();
                    let admitted_producer = producer.clone();
                    actor_seat_producer = admitted_producer.clone();
                    seat_live_task = tokio::spawn(async move {
                        let reproof = seat::watch_live_history(&live_client, handoff.stream, &producer, handoff.tip_seq, handoff.tip_hash).await;
                        let _ = live_tx.send((producer, reproof));
                    });
                    seat_thread = Some(admitted_producer.thread_id.clone());
                    let next_engine_seq = client.seat_delivery.lock().await.next_engine_seq;
                    follow.extend(core.dispatch(RyeOsEvent::PresentationProducerReady { producer: admitted_producer, next_engine_seq }));
                    seat_synced = core.seat.events().len();
                    seat_sync_inflight = false;

                } else if client.seat_live_handoff.lock().await.is_some() {
                    // An effect produced a claimed stream but the reducer did
                    // not admit its replacement session. Never leave that
                    // producer live without an observing terminal core.
                    client.seat_delivery.lock().await.blocked = true;
                    *client.ui_producer.write().expect("producer lock poisoned") = None;
                    terminal_error = Some(std::io::Error::other("replacement live history was not admitted by the terminal core").into());
                    break;
                }
                spawn_effects(&client, follow, &effect_tx);
                dirty = true;
            }
            Some(ack) = seat_ack_rx.recv() => {
                match ack {
                    SeatIoAck::Append { producer, events, outcome, canonical, upto } => {
                        let current = client.ui_producer.read().expect("producer lock poisoned").clone();
                        if !seat_ack_is_current(current.as_ref(), &producer, seat_thread.as_deref()) {
                            continue;
                        }
                        let thread_id = producer.thread_id.clone();
                        if outcome == seat::SeatAppendOutcome::Acknowledged
                            && seat_thread.as_deref() == Some(thread_id.as_str())
                        {
                            let follow = match core.acknowledge_canonical_seat_prefix(&producer, events, canonical) {
                                Ok(effects) => effects,
                                Err(error) => {
                                    client.seat_delivery.lock().await.blocked = true;
                                    terminal_error = Some(std::io::Error::other(error).into());
                                    break;
                                }
                            };
                            if core.acknowledged_seat_prefix_len() == upto {
                                seat_sync_inflight = false;
                                seat_synced = upto;
                                spawn_effects(&client, follow, &effect_tx);
                                dirty = true;
                                queue_seat_sync(
                                    &client,
                                    &core,
                                    &seat_thread,
                                    seat_synced,
                                    &mut seat_sync_inflight,
                                    seat_touch_inflight,
                                    &seat_write_tx,
                                );
                            } else if invalidate_seat_generation(
                                &mut seat_thread,
                                &thread_id,
                            ) {
                                *client.ui_producer.write().expect("producer lock poisoned") = None;
                            }
                        } else if matches!(
                            outcome,
                            seat::SeatAppendOutcome::Unknown | seat::SeatAppendOutcome::Refused
                        ) {
                            // Both outcomes fence this incarnation. Unknown
                            // retains the exact request and cannot be promoted
                            // to an authenticated refusal or a fresh producer.
                            if invalidate_seat_generation(
                                &mut seat_thread,
                                &thread_id,
                            ) {
                                *client.ui_producer.write().expect("producer lock poisoned") = None;
                            }
                        }
                    }
                    SeatIoAck::Touch { producer, outcome } => {
                        seat_touch_inflight = false;
                        let current = client.ui_producer.read().expect("producer lock poisoned").clone();
                        if !seat_ack_is_current(current.as_ref(), &producer, seat_thread.as_deref()) {
                            continue;
                        }
                        if outcome == seat::SeatTouchOutcome::Refused {
                            if invalidate_seat_generation(
                                &mut seat_thread,
                                &producer.thread_id,
                            ) {
                                *client.ui_producer.write().expect("producer lock poisoned") = None;
                            }
                        } else if outcome == seat::SeatTouchOutcome::Retry {
                            // Unknown delivery is not evidence that this producer
                            // was fenced. Retain it and retry presence renewal on
                            // the next short tick interval.
                            last_seat_touch_ms = now_ms().saturating_sub(59_000);
                        }
                    }
                }
            }
            Some((failed_producer, reproof)) = seat_live_rx.recv() => {
                let current = client.ui_producer.read().expect("producer lock poisoned").clone();
                if reproof && current.as_ref() == Some(&failed_producer) {
                    seat_reproof_pending = true;
                    continue;
                }
                if current.as_ref() == Some(&failed_producer)
                    && invalidate_seat_generation(&mut seat_thread, &failed_producer.thread_id) {
                    *client.ui_producer.write().expect("producer lock poisoned") = None;
                    terminal_error = Some(std::io::Error::other(
                        "authenticated seat history was invalidated or disconnected; local producer fenced"
                    ).into());
                    break;
                }
            }
            Some((origin_chain, event_type, data)) = tail_rx.recv(), if shutdown_deadline.is_none() => {
                // Transport only: forward the raw frame to the shared reducer,
                // which accumulates cognition deltas or refetches the snapshot
                // on a durable milestone. Same path the web client uses. No
                // direct repaint: the activity pulse holds fast ticks while
                // deltas stream, so the tick paints coalesced frames.
                if let (Some(chain_root_id), Some(thread_id)) = (tail_chain.as_deref(), tail_thread.as_deref())
                    && let Some(payload) = focused_tail_payload(&origin_chain, chain_root_id, thread_id, &data)
                {
                    let effects = core.dispatch(RyeOsEvent::ThreadTail {
                        thread_id: thread_id.to_string(),
                        event_type,
                        payload,
                    });
                    spawn_effects(&client, effects, &effect_tx);
                }
            }
            Some(message) = session_rx.recv(), if shutdown_deadline.is_none() => {
                match message {
                    hints::SessionMessage::Hint { kind, payload } => {
                        let effects = core.dispatch(RyeOsEvent::HintReceived {
                            kind: kind.clone(),
                            payload,
                        });
                        spawn_effects(&client, effects, &effect_tx);
                        if pending_hints.is_empty() {
                            hint_batch_started_ms = Some(now_ms());
                        }
                        pending_hints.insert(kind);
                        dirty = true;
                    }
                    hints::SessionMessage::DaemonEvent { payload } => {
                        let effects = core.dispatch(RyeOsEvent::DaemonEvent { payload });
                        spawn_effects(&client, effects, &effect_tx);
                        dirty = true;
                    }
                    hints::SessionMessage::Resubscribed => {
                        // The stream was down for a while; every hint from
                        // the gap is lost. One full refetch instead of
                        // trusting whatever the screen froze on.
                        let effects = core.initial_effects();
                        spawn_effects(&client, effects, &effect_tx);
                        pending_hints.clear();
                        hint_batch_started_ms = None;
                        dirty = true;
                    }
                }
            }
            _ = tick.tick(), if shutdown_deadline.is_none() => {
                let tick_now = now_ms();
                if tick_now.saturating_sub(last_seat_touch_ms) >= 60_000 {
                    let (pending_append, delivery_blocked) = {
                        let delivery = client.seat_delivery.lock().await;
                        (delivery.pending.is_some(), delivery.blocked)
                    };
                    if may_start_seat_touch(
                        seat_sync_inflight,
                        seat_touch_inflight,
                        pending_append,
                        delivery_blocked,
                    ) {
                        last_seat_touch_ms = tick_now;
                        if let Some(producer) = client
                            .ui_producer
                            .read()
                            .expect("producer lock poisoned")
                            .clone()
                            .filter(|producer| {
                                seat_thread.as_deref() == Some(producer.thread_id.as_str())
                            })
                        {
                            seat_touch_inflight = true;
                            let client = client.clone();
                            let seat_touch_ack_tx = seat_touch_ack_tx.clone();
                            tokio::spawn(async move {
                                let outcome = seat::touch_seat_thread(&client, &producer).await;
                                let _ = seat_touch_ack_tx
                                    .send(SeatIoAck::Touch { producer, outcome });
                            });
                        }
                    }
                }
                if hint_batch_started_ms
                    .is_some_and(|started| tick_now.saturating_sub(started) >= HINT_FLUSH_MS)
                {
                    let hints = std::mem::take(&mut pending_hints);
                    hint_batch_started_ms = None;
                    let effects = core.dispatch(RyeOsEvent::HintFlushBatch {
                        kinds: hints.into_iter().collect(),
                    });
                    spawn_effects(&client, effects, &effect_tx);
                }
                // Flush a debounced live-filter refetch once typing has settled.
                if feed_dirty {
                    feed_dirty = false;
                    let effects = core.refresh_focused_feeds();
                    spawn_effects(&client, effects, &effect_tx);
                }
                // Step the scene clock exactly once per on-time tick (see
                // `advance_scene_frame`): the fast tick period equals the
                // scene cadence, so re-flooring wall time here would alias
                // into held frames and double steps.
                core.advance_scene_frame(tick_now);
                let effects = core.dispatch(RyeOsEvent::Tick { now_ms: tick_now });
                spawn_effects(&client, effects, &effect_tx);
                queue_seat_sync(
                    &client,
                    &core,
                    &seat_thread,
                    seat_synced,
                    &mut seat_sync_inflight,
                    seat_touch_inflight,
                    &seat_write_tx,
                );
                dirty = true;
            }
            _ = async {
                if let Some(deadline) = shutdown_deadline {
                    tokio::time::sleep_until(deadline).await;
                }
            }, if shutdown_deadline.is_some() => break,
        }
    }

    // Give the terminal back BEFORE settling the seat: close is a best-effort
    // session-route round trip and must never hold the raw alternate screen
    // hostage on a slow daemon. Drop the input stream first so a keystroke
    // typed at the restored shell isn't eaten by the reader.
    drop(events.take());
    drop(term.take());
    // Observation subscriptions must not outlive this presentation loop.
    // These handles only read SSE; effect/append reconciliation is separate.
    session_task.abort();
    if let Some(task) = tail_task.take() {
        task.abort();
    }
    seat_live_task.abort();
    if let Some(task) = seat_reproof_task.take() {
        task.abort();
    }
    if let Some(thread_id) = &seat_thread {
        if may_close_mirrored_seat(
            core.seat.events().len(),
            seat_synced,
            seat_sync_inflight,
            seat_touch_inflight,
            !core.pending_effects.is_empty(),
        ) && terminal_error.is_none()
        {
            // The terminal is already restored above, so waiting here holds
            // nothing hostage. The exact acknowledged prefix is complete.
            seat::relinquish_seat_thread(&client, thread_id).await;
        } else {
            eprintln!(
                "terminal seat {thread_id} was not closed: local history or seat I/O remains unsettled"
            );
        }
    }

    if let Some(error) = terminal_error {
        return Err(error);
    }
    Ok(())
}

/// A frame clock that skips missed ticks: a stalled loop resumes on the
/// next beat instead of replaying the backlog in a burst.
fn frame_clock(period_ms: u64) -> tokio::time::Interval {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(period_ms));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick
}

/// Hand the braid writer the next unmirrored slice, at most one batch in
/// flight. A failed batch fences this writer; it never creates replacement
/// authority or marks an unknown append acknowledged.
fn queue_seat_sync(
    client: &DaemonClient,
    core: &RyeOsCore,
    seat_thread: &Option<String>,
    synced: usize,
    inflight: &mut bool,
    touch_inflight: bool,
    tx: &tokio::sync::mpsc::UnboundedSender<(
        ryeos_client_base::ui::binding::UiPresentationProducer,
        Vec<ryeos_client_base::ui::SeatEvent>,
        usize,
    )>,
) {
    if *inflight || touch_inflight {
        return;
    }
    let Some(thread_id) = seat_thread else {
        return;
    };
    let events = core.seat.events();
    if events.len() <= synced {
        return;
    }
    let Some(producer) = client
        .ui_producer
        .read()
        .expect("producer lock poisoned")
        .clone()
        .filter(|producer| producer.thread_id == *thread_id)
    else {
        return;
    };
    let Ok(binding) = core.data.session.as_ref().ok_or(()).and_then(|session| {
        retained_seat_binding(session, &producer.presentation_id).map_err(|_| ())
    }) else {
        return;
    };
    let count =
        seat::append_prefix_len(&events[synced..], &producer, binding.max_seat_append_bytes);
    // An individually oversized event is handed intact to the writer, which
    // retains the local log and definitively blocks instead of dropping it.
    let count = count.max(1);
    let batch = events[synced..synced + count].to_vec();
    if tx.send((producer, batch, synced + count)).is_ok() {
        *inflight = true;
    }
}

fn reproof_reply_is_current(
    active_generation: Option<uuid::Uuid>,
    reply_generation: uuid::Uuid,
    current_producer: Option<&ryeos_client_base::ui::binding::UiPresentationProducer>,
    reply_producer: &ryeos_client_base::ui::binding::UiPresentationProducer,
) -> bool {
    active_generation == Some(reply_generation) && current_producer == Some(reply_producer)
}

fn seat_ack_is_current(
    current: Option<&ryeos_client_base::ui::binding::UiPresentationProducer>,
    acknowledged: &ryeos_client_base::ui::binding::UiPresentationProducer,
    thread_id: Option<&str>,
) -> bool {
    current == Some(acknowledged) && thread_id == Some(acknowledged.thread_id.as_str())
}

/// Invalidate only the seat generation that produced a failed asynchronous
/// acknowledgement. A late failure from an older generation must never tear
/// down the current seat. An unknown delivery does not authorize a replacement
/// producer or replaying the local log under fresh authority.
fn invalidate_seat_generation(current_thread: &mut Option<String>, failed_thread_id: &str) -> bool {
    if current_thread.as_deref() != Some(failed_thread_id) {
        return false;
    }
    *current_thread = None;
    true
}

fn viewport(width: u16, height: u16) -> BrowserViewport {
    BrowserViewport {
        width: width as u32,
        height: height as u32,
        device_pixel_ratio: 1.0,
    }
}

/// The chain stream can outlive a route-head change, and aborting a replaced
/// task does not drain frames already in the channel. Only the event's own
/// retained thread identity may address the shared live-tail reducer.
fn focused_tail_payload(
    origin_chain: &str,
    current_chain: &str,
    current_thread: &str,
    data: &str,
) -> Option<serde_json::Value> {
    if origin_chain != current_chain {
        return None;
    }
    let payload = serde_json::from_str::<serde_json::Value>(data).ok()?;
    (payload.get("chain_root_id")?.as_str()? == current_chain
        && payload.get("thread_id")?.as_str()? == current_thread)
        .then_some(payload)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_tail_frame_never_inherits_a_new_route_identity() {
        let previous = serde_json::json!({"chain_root_id":"chain-a","thread_id":"thread-a","payload":{"delta":"old"}}).to_string();
        assert!(focused_tail_payload("chain-a", "chain-a", "thread-a", &previous).is_some());
        assert!(focused_tail_payload("chain-a", "chain-a", "thread-b", &previous).is_none());
        assert!(focused_tail_payload("chain-a", "chain-b", "thread-a", &previous).is_none());
        assert!(focused_tail_payload("chain-a", "chain-a", "thread-a", "{}").is_none());
    }

    #[test]
    fn touch_never_overlaps_pending_or_inflight_append_delivery() {
        assert!(may_start_seat_touch(false, false, false, false));
        assert!(!may_start_seat_touch(true, false, false, false));
        assert!(!may_start_seat_touch(false, true, false, false));
        assert!(!may_start_seat_touch(false, false, true, false));
        assert!(!may_start_seat_touch(false, false, false, true));
    }

    #[test]
    fn native_close_never_overtakes_a_local_unmirrored_prefix() {
        assert!(may_close_mirrored_seat(3, 3, false, false, false));
        assert!(!may_close_mirrored_seat(4, 3, false, false, false));
        assert!(!may_close_mirrored_seat(3, 3, true, false, false));
        assert!(!may_close_mirrored_seat(3, 3, false, true, false));
        assert!(!may_close_mirrored_seat(3, 3, false, false, true));
        assert!(!may_close_mirrored_seat(2, 3, false, false, false));
    }

    #[test]
    fn append_ack_requires_exact_producer_not_just_same_thread() {
        let producer = ryeos_client_base::ui::binding::UiPresentationProducer {
            presentation_id: "presentation-a".into(),
            thread_id: "seat-same".into(),
            producer_incarnation: "aa".repeat(32),
        };
        assert!(seat_ack_is_current(
            Some(&producer),
            &producer,
            Some("seat-same")
        ));
        let mut predecessor = producer.clone();
        predecessor.producer_incarnation = "bb".repeat(32);
        assert!(!seat_ack_is_current(
            Some(&producer),
            &predecessor,
            Some("seat-same")
        ));
        predecessor = producer.clone();
        predecessor.presentation_id = "presentation-b".into();
        assert!(!seat_ack_is_current(
            Some(&producer),
            &predecessor,
            Some("seat-same")
        ));
        assert!(!seat_ack_is_current(None, &producer, Some("seat-same")));
        assert!(!seat_ack_is_current(
            Some(&producer),
            &producer,
            Some("seat-other")
        ));
    }

    #[test]
    fn failed_current_seat_preserves_watermark_without_replacement() {
        let mut current = Some("seat-old".to_string());
        let synced = 12;
        let inflight = true;
        assert!(invalidate_seat_generation(&mut current, "seat-old",));
        assert_eq!(current, None);
        assert_eq!(synced, 12);
        assert!(inflight);
    }

    #[test]
    fn late_failure_cannot_invalidate_replacement_seat() {
        let mut current = Some("seat-new".to_string());
        let synced = 3;
        let inflight = true;
        assert!(!invalidate_seat_generation(&mut current, "seat-old",));
        assert_eq!(current.as_deref(), Some("seat-new"));
        assert_eq!(synced, 3);
        assert!(inflight);
    }
}
#[test]
fn reproof_late_reply_cannot_replace_successor_or_fail_changed_producer() {
    let old = uuid::Uuid::new_v4();
    let successor = uuid::Uuid::new_v4();
    let producer = ryeos_client_base::ui::binding::UiPresentationProducer {
        presentation_id: "presentation-a".into(),
        thread_id: "seat-a".into(),
        producer_incarnation: "aa".repeat(32),
    };
    let mut replacement = producer.clone();
    replacement.producer_incarnation = "bb".repeat(32);
    assert!(reproof_reply_is_current(
        Some(old),
        old,
        Some(&producer),
        &producer
    ));
    assert!(!reproof_reply_is_current(
        Some(successor),
        old,
        Some(&producer),
        &producer
    ));
    assert!(!reproof_reply_is_current(
        None,
        old,
        Some(&producer),
        &producer
    ));
    assert!(!reproof_reply_is_current(
        Some(old),
        old,
        Some(&replacement),
        &producer
    ));
    assert!(!reproof_reply_is_current(Some(old), old, None, &producer));
    assert!(reproof_reply_is_current(
        Some(successor),
        successor,
        Some(&replacement),
        &replacement
    ));
}
