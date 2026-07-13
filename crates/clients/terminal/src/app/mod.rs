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

use std::collections::HashMap;
use std::sync::Arc;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use ryeos_client_base::surface::LoadedSurface;
use ryeos_client_base::ui::scene_model::SCENE_FRAME_MS;
use ryeos_client_base::ui::{
    BrowserSession, BrowserViewport, RyeOsCore, RyeOsEffectResult, RyeOsEvent,
};

use crate::render::RyeOsTerminalRenderer;
use crate::terminal::TerminalGuard;
use crate::transport::daemon::DaemonClient;

use effects::spawn_effects;
use keys::{handle_key, should_quit};

pub async fn run(
    project_path: &str,
    read_only: bool,
    loaded_surface: LoadedSurface,
    diagnostics: Vec<String>,
    client: Option<DaemonClient>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Reuse the connection the surface was resolved over (discovery and
    // audience already settled there); connect fresh only without one.
    let mut client = match client {
        Some(client) => client,
        None => DaemonClient::try_connect().await?,
    };
    let surface_ref = loaded_surface
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

    let start_effects = core.dispatch(RyeOsEvent::Start {
        session: session_for(project_path, read_only, &loaded_surface),
        viewport: viewport(width, height),
        now_ms: now_ms(),
    });
    // The seed count marks the surface-declared seat facets; the deferred
    // reattach below compares against it to detect a lost race with live
    // local writes.
    let seeded_events = core.seat.events().len();

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

    client
        .mint_ui_session(&surface_ref, Some(project_path), read_only)
        .await?;
    let client = Arc::new(client);

    // Effect results come home over this channel: batches run off the
    // loop, folds happen on arrival, and a round trip in flight never
    // blocks a frame.
    let (effect_tx, mut effect_rx) =
        tokio::sync::mpsc::unbounded_channel::<Vec<RyeOsEffectResult>>();
    spawn_effects(&client, project_path_of(&core), start_effects, &effect_tx);

    // The seat is itself a thread: braided, owned, replayable. Reattach
    // to the latest running owned seat for this surface when possible;
    // otherwise open one. The round trips run off the loop; the result
    // folds in on arrival. Local seat events mirror into the braid as
    // they append, and the thread settles on clean exit.
    let (seat_tx, mut seat_rx) = tokio::sync::mpsc::unbounded_channel::<seat::SeatBootstrap>();
    {
        let client = client.clone();
        let surface_ref = surface_ref.clone();
        let seat_tx = seat_tx.clone();
        tokio::spawn(async move {
            let _ = seat_tx.send(seat::bootstrap_seat(&client, &surface_ref).await);
        });
    }
    let mut seat_thread: Option<String> = None;
    let mut seat_synced: usize = 0;
    // One braid writer, one batch in flight: mirrored order matches local
    // append order, and the loop never waits on the mirror.
    let (seat_write_tx, mut seat_write_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Vec<serde_json::Value>, usize)>();
    let (seat_ack_tx, mut seat_ack_rx) = tokio::sync::mpsc::unbounded_channel::<(bool, usize)>();
    {
        let client = client.clone();
        tokio::spawn(async move {
            while let Some((thread_id, batch, upto)) = seat_write_rx.recv().await {
                let ok = seat::append_braid(&client, &thread_id, batch).await;
                if seat_ack_tx.send((ok, upto)).is_err() {
                    break;
                }
            }
        });
    }
    let mut seat_sync_inflight = false;

    // Session hints: transient "look" signals; bound views declaring
    // `refresh.on_hint` refetch their sources. This replaces polling —
    // content decides its own liveness.
    let (session_tx, mut session_rx) =
        tokio::sync::mpsc::unbounded_channel::<hints::SessionMessage>();
    hints::spawn_session_listener(client.clone(), session_tx.clone());

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
    let mut pending_hints: HashMap<String, serde_json::Value> = HashMap::new();
    let mut last_hint_flush_ms = 0;

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
                        spawn_effects(&client, project_path_of(&core), effects, &effect_tx);
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
                        spawn_effects(&client, project_path_of(&core), effects, &effect_tx);
                        dirty = true;
                    }
                    _ => {}
                }
            }
            Some(results) = effect_rx.recv() => {
                // Fold one batch's results in emission order; any effects
                // the folds emit form the next spawned generation.
                let mut follow = Vec::new();
                for result in results {
                    follow.extend(core.dispatch(RyeOsEvent::EffectResult { result }));
                }
                spawn_effects(&client, project_path_of(&core), follow, &effect_tx);
                dirty = true;
            }
            Some((ok, upto)) = seat_ack_rx.recv() => {
                seat_sync_inflight = false;
                if ok {
                    seat_synced = upto;
                }
                queue_seat_sync(
                    &core,
                    &seat_thread,
                    seat_synced,
                    &mut seat_sync_inflight,
                    &seat_write_tx,
                );
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
                    spawn_effects(&client, project_path_of(&core), effects, &effect_tx);
                }
            }
            Some(message) = session_rx.recv() => {
                match message {
                    hints::SessionMessage::Hint { kind, payload } => {
                        pending_hints.insert(kind, payload);
                    }
                    hints::SessionMessage::DaemonEvent { payload } => {
                        let effects = core.dispatch(RyeOsEvent::DaemonEvent { payload });
                        spawn_effects(&client, project_path_of(&core), effects, &effect_tx);
                        dirty = true;
                    }
                }
            }
            Some(bootstrap) = seat_rx.recv() => {
                if bootstrap.replayed.is_empty() {
                    // Fresh (or facet-less) braid: adopt it and mirror the
                    // local log from the top, seeds included.
                    seat_thread = bootstrap.thread_id;
                    queue_seat_sync(
                        &core,
                        &seat_thread,
                        seat_synced,
                        &mut seat_sync_inflight,
                        &seat_write_tx,
                    );
                } else if core.seat.events().len() == seeded_events {
                    for event in bootstrap.replayed {
                        core.seat.append_replayed(event);
                    }
                    seat_thread = bootstrap.thread_id;
                    seat_synced = core.seat.events().len();
                    dirty = true;
                } else {
                    // The reattach lost the race with live local writes. The
                    // fold is position-ordered, so replaying history now would
                    // fold stale facets over fresher ones — leave that braid
                    // alone and open a new seat for this session instead.
                    let client = client.clone();
                    let surface_ref = surface_ref.clone();
                    let seat_tx = seat_tx.clone();
                    tokio::spawn(async move {
                        let thread_id = seat::open_seat_thread(&client, &surface_ref).await;
                        let _ = seat_tx.send(seat::SeatBootstrap { thread_id, replayed: Vec::new() });
                    });
                }
            }
            _ = tick.tick() => {
                let tick_now = now_ms();
                if !pending_hints.is_empty()
                    && tick_now.saturating_sub(last_hint_flush_ms) >= HINT_FLUSH_MS
                {
                    last_hint_flush_ms = tick_now;
                    let hints = std::mem::take(&mut pending_hints);
                    let mut effects = Vec::new();
                    for (kind, payload) in hints {
                        effects.extend(core.note_hint(&kind, &payload));
                    }
                    spawn_effects(&client, project_path_of(&core), effects, &effect_tx);
                }
                // Flush a debounced live-filter refetch once typing has settled.
                if feed_dirty {
                    feed_dirty = false;
                    let effects = core.refresh_focused_feeds();
                    spawn_effects(&client, project_path_of(&core), effects, &effect_tx);
                }
                // Step the scene clock exactly once per on-time tick (see
                // `advance_scene_frame`): the fast tick period equals the
                // scene cadence, so re-flooring wall time here would alias
                // into held frames and double steps.
                core.advance_scene_frame(tick_now);
                let effects = core.dispatch(RyeOsEvent::Tick { now_ms: tick_now });
                spawn_effects(&client, project_path_of(&core), effects, &effect_tx);
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

    // Give the terminal back BEFORE settling the seat: the close is one
    // best-effort daemon round trip on `/execute`, which deliberately has
    // no total request timeout, so it must never hold the raw alternate
    // screen hostage on a slow daemon. Drop the input stream first so a
    // keystroke typed at the restored shell isn't eaten by the reader.
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

fn project_path_of(core: &RyeOsCore) -> Option<String> {
    core.data
        .session
        .as_ref()
        .and_then(|session| session.project_path.clone())
}

/// Hand the braid writer the next unmirrored slice, at most one batch in
/// flight. A failed batch retries on the next nudge because the synced
/// watermark only advances on the writer's ack.
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

fn session_for(
    project_path: &str,
    read_only: bool,
    loaded_surface: &LoadedSurface,
) -> BrowserSession {
    let surface_ref = loaded_surface
        .requested_ref()
        .map(str::to_string)
        .unwrap_or_else(|| loaded_surface.spec().name.clone());
    BrowserSession {
        session_id: format!("terminal:{}", now_ms()),
        surface_ref,
        user_principal_id: None,
        effective_surface: Some(serde_json::to_value(loaded_surface.spec()).unwrap_or_default()),
        project_path: Some(project_path.to_string()),
        read_only,
        granted_caps: Vec::new(),
        events_url: None,
    }
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
