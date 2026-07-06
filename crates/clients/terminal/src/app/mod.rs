//! The terminal seat runtime: one event loop folding terminal input,
//! braid tails, session hints, and ticks into the shared `StudioCore`.
//!
//! This module is the loop and nothing else. Effect execution lives in
//! `effects`, seat-thread lifecycle in `seat`, live-signal subscriptions
//! in `hints`, key translation in `keys`. Rendering is `crate::render`.

mod effects;
mod hints;
mod keys;
mod seat;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use ryeos_client_base::studio::{BrowserSession, BrowserViewport, StudioCore, StudioEvent};
use ryeos_client_base::surface::LoadedSurface;

use crate::render::StudioTerminalRenderer;
use crate::terminal::TerminalGuard;
use crate::transport::daemon::DaemonClient;

use effects::dispatch_effects;
use keys::{handle_key, should_quit};
use seat::sync_seat_braid;

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
    let mut renderer = StudioTerminalRenderer::new();
    let mut core = StudioCore::default();
    let mut dirty = true;

    // Degraded live timeline: tail the route's head thread over SSE.
    // The braid is the truth; this is it arriving now. Retargets abort
    // the old tail and open a new one.
    let (tail_tx, mut tail_rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    let mut tail_chain: Option<String> = None;
    let mut tail_task: Option<tokio::task::JoinHandle<()>> = None;

    let start_effects = core.dispatch(StudioEvent::Start {
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
        core.notice(
            message,
            ryeos_client_base::studio::view_model::StudioTone::Warn,
        );
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
    let client = std::sync::Arc::new(client);

    dispatch_effects(&mut core, &client, start_effects).await;

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

    // Session hints: transient "look" signals; bound views declaring
    // `refresh.on_hint` refetch their sources. This replaces polling —
    // content decides its own liveness.
    let (hint_tx, mut hint_rx) = tokio::sync::mpsc::unbounded_channel::<hints::HintMessage>();
    hints::spawn_hint_listener(client.clone(), hint_tx.clone());

    let mut events = EventStream::new();
    // Adaptive frame clock: the backdrop's breathe/sweep animation reads
    // fluid at ~8fps, so the tick quickens while the center is empty (the
    // diff renderer repaints only the handful of changed cells). With
    // tiles up nothing animates per-tick, so 4fps keeps the debounce and
    // seat sync cadence without burning cycles.
    const TICK_BACKDROP_MS: u64 = 120;
    const TICK_TILES_MS: u64 = 250;
    let mut tick_ms = TICK_TILES_MS;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(tick_ms));
    // A live-filter edit defers its list refetch to the tick (debounce), so
    // typing a filter never blocks on a per-keystroke daemon round-trip.
    let mut feed_dirty = false;

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
            tick = tokio::time::interval(std::time::Duration::from_millis(tick_ms));
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
                        dispatch_effects(&mut core, &client, effects).await;
                        sync_seat_braid(&client, &core, &seat_thread, &mut seat_synced).await;
                        dirty = true;
                    }
                    Event::Resize(w, h) => {
                        let _ = term.update_size();
                        let effects = core.dispatch(StudioEvent::Resize { viewport: viewport(w, h) });
                        dispatch_effects(&mut core, &client, effects).await;
                        dirty = true;
                    }
                    _ => {}
                }
            }
            Some((event_type, data)) = tail_rx.recv() => {
                // Transport only: forward the raw frame to the shared reducer,
                // which accumulates cognition deltas or refetches the snapshot
                // on a durable milestone. Same path the web client uses.
                if let Some(thread_id) = tail_thread.clone() {
                    let payload = serde_json::from_str::<serde_json::Value>(&data)
                        .unwrap_or(serde_json::Value::Null);
                    let effects = core.dispatch(StudioEvent::ThreadTail {
                        thread_id,
                        event_type,
                        payload,
                    });
                    dispatch_effects(&mut core, &client, effects).await;
                    dirty = true;
                }
            }
            Some((kind, payload)) = hint_rx.recv() => {
                let effects = core.note_hint(&kind, &payload);
                if !effects.is_empty() {
                    dispatch_effects(&mut core, &client, effects).await;
                    dirty = true;
                }
            }
            Some(bootstrap) = seat_rx.recv() => {
                if bootstrap.replayed.is_empty() {
                    // Fresh (or facet-less) braid: adopt it and mirror the
                    // local log from the top, seeds included.
                    seat_thread = bootstrap.thread_id;
                    sync_seat_braid(&client, &core, &seat_thread, &mut seat_synced).await;
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
                // Flush a debounced live-filter refetch once typing has settled.
                if feed_dirty {
                    feed_dirty = false;
                    let effects = core.refresh_focused_feeds();
                    dispatch_effects(&mut core, &client, effects).await;
                }
                let effects = core.dispatch(StudioEvent::Tick { now_ms: now_ms() });
                dispatch_effects(&mut core, &client, effects).await;
                sync_seat_braid(&client, &core, &seat_thread, &mut seat_synced).await;
                dirty = true;
            }
        }
    }

    if let Some(thread_id) = &seat_thread {
        seat::close_seat_thread(&client, thread_id).await;
    }

    Ok(())
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
