//! The terminal seat runtime: one event loop folding terminal input,
//! braid tails, session hints, and ticks into the shared `RyeOsCore`.
//!
//! This module is the loop and nothing else. Effect execution lives in
//! `effects`, seat-thread lifecycle in `seat`, live-signal subscriptions
//! in `hints`, key translation in `keys`. Rendering is `crate::render`.

mod effects;
mod hints;
mod keys;
mod seat;

use std::collections::HashSet;
use std::sync::Arc;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use ryeos_client_base::surface::LoadedSurface;
use ryeos_client_base::ui::scene_model::SCENE_FRAME_MS;
use ryeos_client_base::ui::{BrowserViewport, RyeOsCore, RyeOsEffectResult, RyeOsEvent};

use crate::render::RyeOsTerminalRenderer;
use crate::terminal::TerminalGuard;
use crate::transport::daemon::DaemonClient;

use effects::spawn_effects;
use keys::{handle_key, should_quit};

enum SeatIoAck {
    Append {
        thread_id: String,
        ok: bool,
        upto: usize,
    },
    Touch {
        thread_id: String,
        ok: bool,
    },
}

const SEAT_RECONNECT_MIN_MS: u64 = 1_000;
const SEAT_RECONNECT_MAX_MS: u64 = 30_000;

pub async fn run(
    project_path: &str,
    loaded_surface: LoadedSurface,
    diagnostics: Vec<String>,
    client: Option<DaemonClient>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Reuse the connection the surface was resolved over (discovery and
    // audience already settled there); connect fresh only without one.
    let client = match client {
        Some(client) => client,
        None => DaemonClient::try_connect().await?,
    };
    let mut surface_ref = loaded_surface
        .requested_ref()
        .map(str::to_string)
        .unwrap_or_else(|| loaded_surface.spec().name.clone());

    let mut term = TerminalGuard::init()?;
    let (width, height) = term.size();
    let mut stdout = std::io::stdout();
    let mut renderer = RyeOsTerminalRenderer::new();
    let mut core = RyeOsCore::default();
    let mut dirty = true;

    // Degraded live timeline: tail the route's head thread over SSE.
    // The braid is the truth; this is it arriving now. Retargets abort
    // the old tail and open a new one.
    let (tail_tx, mut tail_rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    let mut tail_chain: Option<String> = None;
    let mut tail_task: Option<tokio::task::JoinHandle<()>> = None;

    client
        .mint_ui_session(&surface_ref, Some(project_path))
        .await?;
    let current_session = client.current_ui_session().await?;
    let session = current_session;
    let start_effects = core.dispatch(RyeOsEvent::Start {
        session,
        viewport: viewport(width, height),
        now_ms: now_ms(),
    });
    // The seed count marks the surface-declared seat facets; the deferred
    // reattach below compares against it to detect a lost race with live
    // local writes.
    let mut seeded_events = core.seat.events().len();

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
        renderer.render(&mut stdout, &vm, width, height)?;
    }

    let client = Arc::new(client);

    // Effect results come home over this channel: batches run off the
    // loop, folds happen on arrival, and a round trip in flight never
    // blocks a frame.
    let (effect_tx, mut effect_rx) =
        tokio::sync::mpsc::unbounded_channel::<Vec<RyeOsEffectResult>>();
    spawn_effects(&client, start_effects, &effect_tx);

    // The seat is itself a thread: braided, owned, replayable. Reattach
    // to the latest running owned seat for this surface when possible;
    // otherwise open one. The round trips run off the loop; the result
    // folds in on arrival. Local seat events mirror into the braid as
    // they append, and the thread settles on clean exit.
    let (seat_tx, mut seat_rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<seat::SeatBootstrap, String>>();
    {
        let client = client.clone();
        let surface_ref = surface_ref.clone();
        let project_path = project_path.to_string();
        let seat_tx = seat_tx.clone();
        tokio::spawn(async move {
            let _ = seat_tx.send(seat::bootstrap_seat(&client, &surface_ref, &project_path).await);
        });
    }
    let mut seat_thread: Option<String> = None;
    let mut seat_synced: usize = 0;
    let mut seat_bootstrap_inflight = true;
    let mut seat_bootstrap_retry_at_ms: Option<u64> = None;
    let mut seat_reconnect_delay_ms = SEAT_RECONNECT_MIN_MS;
    // One braid writer, one batch in flight: mirrored order matches local
    // append order, and the loop never waits on the mirror.
    let (seat_write_tx, mut seat_write_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Vec<serde_json::Value>, usize)>();
    let (seat_ack_tx, mut seat_ack_rx) = tokio::sync::mpsc::unbounded_channel::<SeatIoAck>();
    let seat_touch_ack_tx = seat_ack_tx.clone();
    {
        let client = client.clone();
        tokio::spawn(async move {
            while let Some((thread_id, batch, upto)) = seat_write_rx.recv().await {
                let ok = seat::append_braid(&client, &thread_id, batch).await;
                if seat_ack_tx
                    .send(SeatIoAck::Append {
                        thread_id,
                        ok,
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

    // Session hints: transient "look" signals; bound views declaring
    // `refresh.on_hint` refetch their sources. This replaces polling —
    // content decides its own liveness.
    let (mut session_tx, mut session_rx) =
        tokio::sync::mpsc::unbounded_channel::<hints::SessionMessage>();
    let mut session_task = hints::spawn_session_listener(client.clone(), session_tx.clone());

    let mut events = EventStream::new();
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

    loop {
        if dirty {
            let vm = core.envelope(Vec::new()).view_model;
            renderer.render(&mut stdout, &vm, term.size().0, term.size().1)?;
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
        let route = core.seat.fold().input_route();
        let desired_chain = route.chain_root.clone().or_else(|| route.thread.clone());
        let desired_thread = route.thread.clone().or_else(|| desired_chain.clone());
        if desired_chain != tail_chain {
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
            maybe_event = events.next() => {
                let Some(Ok(event)) = maybe_event else { break; };
                match event {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if should_quit(&core, &key) {
                            break;
                        }
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
                            &core,
                            &seat_thread,
                            seat_synced,
                            &mut seat_sync_inflight,
                            &seat_write_tx,
                        );
                        dirty = true;
                    }
                    Event::Resize(w, h) => {
                        let _ = term.update_size();
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
                    (
                        session.session_id.clone(),
                        session.surface_ref.clone(),
                        session.project_path.clone().unwrap_or_default(),
                    )
                });
                let current_session_id = current_session
                    .as_ref()
                    .map(|(session_id, _, _)| session_id.clone());
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

                    // Successful activation settles the exact predecessor seat
                    // daemon-side. The successor cookie is intentionally not
                    // authorized to close predecessor-owned threads.
                    seat_thread.take();
                    seat_synced = 0;
                    seat_sync_inflight = false;
                    seat_bootstrap_retry_at_ms = None;
                    seat_reconnect_delay_ms = SEAT_RECONNECT_MIN_MS;
                    seeded_events = core.seat.events().len();

                    if let Some((_, replacement_surface, replacement_project)) = current_session {
                        surface_ref = replacement_surface;
                        let client = client.clone();
                        let replacement_surface = surface_ref.clone();
                        let seat_tx = seat_tx.clone();
                        seat_bootstrap_inflight = true;
                        tokio::spawn(async move {
                            let _ = seat_tx.send(
                                seat::bootstrap_seat(
                                    &client,
                                    &replacement_surface,
                                    &replacement_project,
                                )
                                .await,
                            );
                        });
                    }
                }
                spawn_effects(&client, follow, &effect_tx);
                dirty = true;
            }
            Some(ack) = seat_ack_rx.recv() => {
                match ack {
                    SeatIoAck::Append { thread_id, ok, upto } => {
                        if ok && seat_thread.as_deref() == Some(thread_id.as_str()) {
                            seat_sync_inflight = false;
                            seat_synced = upto;
                            queue_seat_sync(
                                &core,
                                &seat_thread,
                                seat_synced,
                                &mut seat_sync_inflight,
                                &seat_write_tx,
                            );
                        } else if !ok {
                            // The seat may have settled or been lost across a
                            // daemon restart. Never retry the same braid against
                            // a known-bad thread: drop this generation and let
                            // the bounded bootstrap path open/reattach another.
                            invalidate_seat_generation(
                                &mut seat_thread,
                                &thread_id,
                                &mut seat_synced,
                                &mut seat_sync_inflight,
                                &mut seat_bootstrap_retry_at_ms,
                                &mut seat_reconnect_delay_ms,
                                now_ms(),
                            );
                        }
                    }
                    SeatIoAck::Touch { thread_id, ok } => {
                        if !ok {
                            invalidate_seat_generation(
                                &mut seat_thread,
                                &thread_id,
                                &mut seat_synced,
                                &mut seat_sync_inflight,
                                &mut seat_bootstrap_retry_at_ms,
                                &mut seat_reconnect_delay_ms,
                                now_ms(),
                            );
                        }
                    }
                }
            }
            Some((event_type, data)) = tail_rx.recv() => {
                // Transport only: forward the raw frame to the shared reducer,
                // which accumulates cognition deltas or refetches the snapshot
                // on a durable milestone. Same path the web client uses. No
                // direct repaint: the activity pulse holds fast ticks while
                // deltas stream, so the tick paints coalesced frames.
                if let Some(thread_id) = tail_thread.clone() {
                    let payload = serde_json::from_str::<serde_json::Value>(&data)
                        .unwrap_or(serde_json::Value::Null);
                    let effects = core.dispatch(RyeOsEvent::ThreadTail {
                        thread_id,
                        event_type,
                        payload,
                    });
                    spawn_effects(&client, effects, &effect_tx);
                }
            }
            Some(message) = session_rx.recv() => {
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
            Some(bootstrap) = seat_rx.recv() => {
                seat_bootstrap_inflight = false;
                let bootstrap = bootstrap.map_err(|error| {
                    std::io::Error::other(format!("RyeOS seat attach failed: {error}"))
                })?;
                let bootstrap_thread_id = bootstrap.thread_id;
                seat_bootstrap_retry_at_ms = None;
                seat_reconnect_delay_ms = SEAT_RECONNECT_MIN_MS;
                if bootstrap.replayed.is_empty() {
                    // Fresh (or facet-less) braid: adopt it and mirror the
                    // local log from the top, seeds included.
                    seat_thread = Some(bootstrap_thread_id);
                    seat_synced = 0;
                    queue_seat_sync(
                        &core,
                        &seat_thread,
                        seat_synced,
                        &mut seat_sync_inflight,
                        &seat_write_tx,
                    );
                } else if core.seat.events().len() == seeded_events {
                    let replay_effects = core.replay_seat_events(bootstrap.replayed);
                    spawn_effects(&client, replay_effects, &effect_tx);
                    seat_thread = Some(bootstrap_thread_id);
                    seat_synced = core.seat.events().len();
                    dirty = true;
                } else {
                    // The reattach lost the race with live local writes. The
                    // fold is position-ordered, so replaying history now would
                    // fold stale facets over fresher ones — leave that braid
                    // alone and open a new seat for this session instead.
                    let client = client.clone();
                    let surface_ref = surface_ref.clone();
                    let project_path = project_path.to_string();
                    let seat_tx = seat_tx.clone();
                    seat_bootstrap_inflight = true;
                    tokio::spawn(async move {
                        let result = seat::open_seat_thread(&client, &surface_ref, &project_path)
                            .await
                            .map(|thread_id| seat::SeatBootstrap {
                                thread_id,
                                replayed: Vec::new(),
                            });
                        let _ = seat_tx.send(result);
                    });
                }
            }
            _ = tick.tick() => {
                let tick_now = now_ms();
                if seat_thread.is_none()
                    && !seat_bootstrap_inflight
                    && seat_bootstrap_retry_at_ms.is_some_and(|retry_at| tick_now >= retry_at)
                {
                    seat_bootstrap_inflight = true;
                    seat_bootstrap_retry_at_ms = None;
                    let client = client.clone();
                    let surface_ref = surface_ref.clone();
                    let project_path = project_path.to_string();
                    let seat_tx = seat_tx.clone();
                    tokio::spawn(async move {
                        let _ = seat_tx.send(
                            seat::bootstrap_seat(&client, &surface_ref, &project_path).await,
                        );
                    });
                }
                if tick_now.saturating_sub(last_seat_touch_ms) >= 60_000 {
                    last_seat_touch_ms = tick_now;
                    if let Some(thread_id) = seat_thread.clone() {
                        let client = client.clone();
                        let seat_touch_ack_tx = seat_touch_ack_tx.clone();
                        tokio::spawn(async move {
                            let ok = seat::touch_seat_thread(&client, &thread_id).await;
                            let _ = seat_touch_ack_tx.send(SeatIoAck::Touch { thread_id, ok });
                        });
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
                    &core,
                    &seat_thread,
                    seat_synced,
                    &mut seat_sync_inflight,
                    &seat_write_tx,
                );
                dirty = true;
            }
        }
    }

    // Give the terminal back BEFORE settling the seat: close is a best-effort
    // session-route round trip and must never hold the raw alternate screen
    // hostage on a slow daemon. Drop the input stream first so a keystroke
    // typed at the restored shell isn't eaten by the reader.
    drop(events);
    drop(term);
    if let Some(thread_id) = &seat_thread {
        // The terminal is already restored above, so waiting here holds
        // nothing hostage — and abandoning the settle would leave a
        // phantom running seat thread in every listing until the next
        // launch reattaches it. Wait it out.
        seat::close_seat_thread(&client, thread_id).await;
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
/// flight. A failed batch invalidates that seat generation; the replacement
/// seat starts again at watermark zero and receives the full local log.
fn queue_seat_sync(
    core: &RyeOsCore,
    seat_thread: &Option<String>,
    synced: usize,
    inflight: &mut bool,
    tx: &tokio::sync::mpsc::UnboundedSender<(String, Vec<serde_json::Value>, usize)>,
) {
    if *inflight {
        return;
    }
    let Some(thread_id) = seat_thread else {
        return;
    };
    let events = core.seat.events();
    if events.len() <= synced {
        return;
    }
    let batch = seat::braid_batch(&events[synced..]);
    if tx.send((thread_id.clone(), batch, events.len())).is_ok() {
        *inflight = true;
    }
}

/// Invalidate only the seat generation that produced a failed asynchronous
/// acknowledgement. A late failure from an older generation must never tear
/// down the replacement seat. Recovery is tick-driven after a fixed delay, so
/// a dead seat cannot create an unbounded request loop.
fn invalidate_seat_generation(
    current_thread: &mut Option<String>,
    failed_thread_id: &str,
    synced: &mut usize,
    sync_inflight: &mut bool,
    retry_at_ms: &mut Option<u64>,
    retry_delay_ms: &mut u64,
    now_ms: u64,
) -> bool {
    if current_thread.as_deref() != Some(failed_thread_id) {
        return false;
    }
    *current_thread = None;
    *synced = 0;
    *sync_inflight = false;
    schedule_seat_reconnect(retry_at_ms, retry_delay_ms, now_ms);
    true
}

fn schedule_seat_reconnect(retry_at_ms: &mut Option<u64>, delay_ms: &mut u64, now_ms: u64) {
    *retry_at_ms = Some(now_ms.saturating_add(*delay_ms));
    *delay_ms = delay_ms.saturating_mul(2).min(SEAT_RECONNECT_MAX_MS);
}

fn viewport(width: u16, height: u16) -> BrowserViewport {
    BrowserViewport {
        width: width as u32,
        height: height as u32,
        device_pixel_ratio: 1.0,
    }
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
    fn failed_current_seat_resets_watermark_and_schedules_reconnect() {
        let mut current = Some("seat-old".to_string());
        let mut synced = 12;
        let mut inflight = true;
        let mut retry_at = None;
        let mut retry_delay = SEAT_RECONNECT_MIN_MS;

        assert!(invalidate_seat_generation(
            &mut current,
            "seat-old",
            &mut synced,
            &mut inflight,
            &mut retry_at,
            &mut retry_delay,
            500,
        ));
        assert_eq!(current, None);
        assert_eq!(synced, 0);
        assert!(!inflight);
        assert_eq!(retry_at, Some(500 + SEAT_RECONNECT_MIN_MS));
        assert_eq!(retry_delay, 2 * SEAT_RECONNECT_MIN_MS);
    }

    #[test]
    fn late_failure_cannot_invalidate_replacement_seat() {
        let mut current = Some("seat-new".to_string());
        let mut synced = 3;
        let mut inflight = true;
        let mut retry_at = None;
        let mut retry_delay = SEAT_RECONNECT_MIN_MS;

        assert!(!invalidate_seat_generation(
            &mut current,
            "seat-old",
            &mut synced,
            &mut inflight,
            &mut retry_at,
            &mut retry_delay,
            500,
        ));
        assert_eq!(current.as_deref(), Some("seat-new"));
        assert_eq!(synced, 3);
        assert!(inflight);
        assert_eq!(retry_at, None);
        assert_eq!(retry_delay, SEAT_RECONNECT_MIN_MS);
    }

    #[test]
    fn reconnect_backoff_caps_at_thirty_seconds() {
        let mut retry_at = None;
        let mut delay = SEAT_RECONNECT_MAX_MS;
        schedule_seat_reconnect(&mut retry_at, &mut delay, 100);
        assert_eq!(retry_at, Some(100 + SEAT_RECONNECT_MAX_MS));
        assert_eq!(delay, SEAT_RECONNECT_MAX_MS);
    }
}
