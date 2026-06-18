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
use seat::{open_seat_thread, reattach_seat_thread, sync_seat_braid};

pub async fn run(
    project_path: &str,
    read_only: bool,
    loaded_surface: LoadedSurface,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = DaemonClient::try_connect().await?;
    let surface_ref = loaded_surface
        .requested_ref()
        .map(str::to_string)
        .unwrap_or_else(|| loaded_surface.spec().name.clone());
    client
        .mint_ui_session(&surface_ref, Some(project_path), read_only)
        .await?;
    let client = std::sync::Arc::new(client);
    let mut term = TerminalGuard::init()?;
    let (width, height) = term.size();
    let mut stdout = std::io::stdout();
    let mut renderer = StudioTerminalRenderer::new();
    let mut core = StudioCore::default();
    let mut dirty = true;

    // Degraded live timeline: tail the route's head thread over SSE.
    // The braid is the truth; this is it arriving now. Retargets abort
    // the old tail and open a new one.
    let (tail_tx, mut tail_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    let mut tail_thread: Option<String> = None;
    let mut tail_task: Option<tokio::task::JoinHandle<()>> = None;

    let start_effects = core.dispatch(StudioEvent::Start {
        session: session_for(project_path, read_only, &loaded_surface),
        viewport: viewport(width, height),
        now_ms: now_ms(),
    });
    dispatch_effects(&mut core, &client, start_effects).await;

    // The seat is itself a thread: braided, owned, replayable. Reattach
    // to the latest running owned seat for this surface when possible;
    // otherwise open one. Mirror every new local seat event into its
    // braid and settle it on clean exit.
    let seeded_events = core.seat.events().len();
    let reattached = reattach_seat_thread(&client, &mut core).await;
    let replayed_events = (reattached.is_some() && core.seat.events().len() > seeded_events)
        .then(|| core.seat.events().len());
    let mut seat_thread = reattached;
    if seat_thread.is_none() {
        seat_thread = open_seat_thread(&client, &core).await;
    }
    let mut seat_synced: usize = replayed_events.unwrap_or(0);
    sync_seat_braid(&client, &core, &seat_thread, &mut seat_synced).await;

    // Session hints: transient "look" signals; bound views declaring
    // `refresh.on_hint` refetch their sources. This replaces polling —
    // content decides its own liveness.
    let (hint_tx, mut hint_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    hints::spawn_hint_listener(client.clone(), hint_tx.clone());

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));

    loop {
        if dirty {
            let vm = core.envelope(Vec::new()).view_model;
            renderer.render(&mut stdout, &vm, term.size().0, term.size().1)?;
            dirty = false;
        }

        // Reconcile the SSE tail with the route facet.
        let desired = core.seat.fold().input_route().thread;
        if desired != tail_thread {
            if let Some(task) = tail_task.take() {
                task.abort();
            }
            tail_thread = desired.clone();
            if let Some(thread_id) = desired {
                tail_task = Some(hints::spawn_thread_tail(
                    client.clone(),
                    thread_id,
                    tail_tx.clone(),
                ));
            }
        }

        tokio::select! {
            maybe_event = events.next() => {
                let Some(Ok(event)) = maybe_event else { break; };
                match event {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if should_quit(&core, &key) {
                            break;
                        }
                        let effects = handle_key(&mut core, key);
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
            Some(kind) = hint_rx.recv() => {
                let effects = core.effects_for_hint(&kind);
                if !effects.is_empty() {
                    dispatch_effects(&mut core, &client, effects).await;
                    dirty = true;
                }
            }
            _ = tick.tick() => {
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
