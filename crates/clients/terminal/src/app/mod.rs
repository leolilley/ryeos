//! The terminal event loop: terminal input, ticks and shutdown timing around
//! the shared client runtime.
//!
//! This module owns what is terminal-specific: key, paste and resize input,
//! the 250 ms frame clock, the debounced live-filter refetch, input gating
//! while a seat reproof is pending, and the bounded close. Everything that
//! happens off the reducer's turn (the seat, its history and appends, effect
//! execution, session hints and the focused tail) belongs to
//! `ryeos-client-runtime`; the loop is woken by its notify and folds the
//! results with `drain`. Key translation lives in `keys`, rendering in
//! `crate::render`.

mod adapter;
mod keys;

use std::rc::Rc;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use ryeos_client_base::ui::{BrowserSession, BrowserViewport, RyeOsEvent};
use ryeos_client_runtime::ClientRuntime;
use ryeos_client_runtime::RuntimeError;
use ryeos_client_runtime::error::SeatOpenStage;
use ryeos_client_runtime::inbox::WakeFn;
use ryeos_client_runtime::platform::native::NativePlatform;
use ryeos_client_runtime::replay::CoreConfig;
use ryeos_client_runtime::runtime::{Drained, StartMode, StopReason};
use ryeos_client_runtime::seat::RetryPolicy;
use tokio::sync::Notify;

use crate::render::RyeOsTerminalRenderer;
use crate::terminal::TerminalGuard;

use keys::{handle_key, should_quit};

/// How long the loop keeps folding seat and effect results after input ends,
/// so the exact local prefix can be acknowledged before the seat is given back.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(10);

/// Fixed 4fps frame clock: it keeps field playback and the seat cadence
/// without burning cycles.
const TICK_MS: u64 = 250;

pub async fn run(
    diagnostics: Vec<String>,
    platform: Rc<NativePlatform>,
    session: BrowserSession,
    mode: StartMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut term = Some(TerminalGuard::init()?);
    let (width, height) = term.as_ref().expect("terminal initialized").size();
    let mut stdout = std::io::stdout();
    let mut renderer = RyeOsTerminalRenderer::new();
    let mut dirty = true;

    let wake = Rc::new(Notify::new());
    let wake_fn: WakeFn = {
        let wake = wake.clone();
        Rc::new(move || wake.notify_one())
    };
    let mut runtime = ClientRuntime::new(platform, wake_fn, RetryPolicy::TERMINAL)
        .with_adapter(adapter::terminal_adapter());

    // The runtime claims the seat, proves its history and builds the core;
    // nothing else runs before that completes.
    let (mut core, start_effects) = match runtime
        .start(
            session,
            viewport(width, height),
            mode,
            CoreConfig::default(),
        )
        .await
    {
        Ok(started) => started,
        Err(error) => {
            // Restore the terminal first so the diagnostic is readable.
            drop(term.take());
            report_open_seat(&error);
            return Err(error.into());
        }
    };

    // Surface startup diagnostics (surface/view resolution warnings) as in-TUI
    // notices. Otherwise they print to stderr BEFORE the alternate screen is
    // entered, so they scroll off above the render where they can't be read.
    for message in diagnostics {
        core.notice(message, ryeos_client_base::ui::view_model::RyeOsTone::Warn);
    }

    // First frame before any daemon round trip: chrome and docks
    // paint immediately; bound views fill in as sources land.
    {
        let vm = core.envelope(Vec::new()).view_model;
        if let Err(error) = renderer.render(&mut stdout, &vm, width, height) {
            // Producer-ready can finish a replayed prepared operation before
            // this first frame, and no mirror writer exists yet: closing that
            // seat would discard a local, unacknowledged event. Restore the
            // terminal before any cleanup request or diagnostic output.
            drop(term.take());
            if start_effects.is_empty() && runtime.may_close(&core) {
                let _ = runtime.relinquish().await;
            } else if let Some(producer) = runtime.producer() {
                eprintln!(
                    "RyeOS first frame failed; seat {} remains open because local history or effects are unsettled",
                    producer.thread_id
                );
            }
            return Err(error.into());
        }
    }

    runtime.submit(start_effects);
    runtime.after_mutation(&core);

    let mut events = Some(EventStream::new());
    let mut tick = frame_clock(TICK_MS);
    // A live-filter edit defers its list refetch to the tick (debounce), so
    // typing a filter never blocks on a per-keystroke daemon round-trip.
    let mut feed_dirty = false;
    let mut seat = runtime.seat_status();
    let mut terminal_error: Option<Box<dyn std::error::Error>> = None;
    // Once input ends, restore the host terminal immediately. The loop then
    // gets a bounded chance to settle its exact local prefix before any seat
    // close is attempted.
    let mut shutdown_requested = false;
    let mut shutdown_deadline: Option<tokio::time::Instant> = None;

    loop {
        if shutdown_requested {
            shutdown_requested = false;
            events.take();
            term.take();
            shutdown_deadline = Some(tokio::time::Instant::now() + SHUTDOWN_DRAIN);
            // Hints, tail and reproof stop; appends and effects keep settling.
            runtime.begin_close();
            runtime.after_mutation(&core);
        }
        if shutdown_deadline.is_some() && runtime.may_close(&core) {
            break;
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

        tokio::select! {
            maybe_event = async { events.as_mut().expect("input active").next().await }, if events.is_some() => {
                let Some(Ok(event)) = maybe_event else {
                    shutdown_requested = true;
                    continue;
                };
                match event {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if should_quit(&core, &key) {
                            shutdown_requested = true;
                            continue;
                        }
                        if seat.reproof_pending { continue; }
                        let before = core.focused_input_buffer().map(|b| b.text.clone());
                        let effects = handle_key(&mut core, key);
                        // A live-filter edit applies instantly but returns no
                        // effects: its list refetch is debounced to the tick so
                        // typing never blocks on a daemon round-trip per key.
                        if effects.is_empty()
                            && core.key_context().input_is_live_filter
                            && core.focused_input_buffer().map(|b| b.text.clone()) != before
                        {
                            feed_dirty = true;
                        }
                        runtime.submit(effects);
                        runtime.after_mutation(&core);
                        dirty = true;
                    }
                    Event::Paste(text) => {
                        if seat.reproof_pending { continue; }
                        let before = core.focused_input_buffer().map(|b| b.text.clone());
                        let effects = core.insert_focused_input_text(&text);
                        if effects.is_empty()
                            && core.key_context().input_is_live_filter
                            && core.focused_input_buffer().map(|b| b.text.clone()) != before
                        {
                            feed_dirty = true;
                        }
                        runtime.submit(effects);
                        runtime.after_mutation(&core);
                        dirty = true;
                    }
                    Event::Resize(w, h) => {
                        let _ = term.as_mut().expect("terminal active").update_size();
                        let effects = core.dispatch(RyeOsEvent::Resize { viewport: viewport(w, h) });
                        runtime.submit(effects);
                        runtime.after_mutation(&core);
                        dirty = true;
                    }
                    _ => {}
                }
            }
            _ = wake.notified() => {
                // Fold, in order, whatever the runtime's tasks produced: effect
                // results, acknowledgements, hints and tail frames.
                let Drained { effects, changed, seat: status, stop, .. } = runtime.drain(&mut core);
                seat = status;
                dirty |= changed;
                runtime.submit(effects);
                if let Some(stop) = stop {
                    terminal_error = Some(stop_error(stop));
                    break;
                }
            }
            _ = tick.tick(), if shutdown_deadline.is_none() => {
                let tick_now = now_ms();
                // Flush a debounced live-filter refetch once typing has settled.
                if feed_dirty {
                    feed_dirty = false;
                    let effects = core.refresh_focused_feeds();
                    runtime.submit(effects);
                }
                let effects = core.dispatch(RyeOsEvent::Tick { now_ms: tick_now });
                runtime.submit(effects);
                // The runtime renews seat presence on its own cadence from here.
                runtime.after_mutation(&core);
                for message in runtime.take_layout_errors() {
                    core.notice(message, ryeos_client_base::ui::view_model::RyeOsTone::Warn);
                }
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
    // round trip and must never hold the raw alternate screen hostage on a
    // slow daemon. Drop the input stream first so a keystroke typed at the
    // restored shell isn't eaten by the reader.
    drop(events.take());
    drop(term.take());
    runtime.begin_close();
    if terminal_error.is_none()
        && let Some(producer) = runtime.producer()
    {
        if runtime.may_close(&core) {
            // The terminal is already restored above, so waiting here holds
            // nothing hostage. The exact acknowledged prefix is complete.
            if let Err(error) = runtime.relinquish().await {
                eprintln!(
                    "terminal seat {} was not given back: {error}",
                    producer.thread_id
                );
            }
        } else {
            eprintln!(
                "terminal seat {} was not closed: local history or seat I/O remains unsettled",
                producer.thread_id
            );
        }
    }

    if let Some(error) = terminal_error {
        return Err(error);
    }
    Ok(())
}

/// Say, on stderr, which claimed seat start left open and why. The terminal
/// is already restored when this runs.
fn report_open_seat(error: &RuntimeError) {
    let RuntimeError::SeatLeftOpen {
        thread_id, stage, ..
    } = error
    else {
        return;
    };
    match stage {
        SeatOpenStage::ReplayAcceptance => eprintln!(
            "RyeOS retained replay failed; seat {thread_id} remains open because replay acceptance is uncertain"
        ),
        SeatOpenStage::ProducerReadiness => eprintln!(
            "RyeOS producer readiness refused for seat {thread_id}; claimed owner remains lifecycle-owned"
        ),
    }
}

/// The error a stopped seat ends the session with. The terminal has no
/// recovery interface, so every stop ends the loop.
fn stop_error(stop: StopReason) -> Box<dyn std::error::Error> {
    let message = match stop {
        StopReason::Fatal(message) | StopReason::Fenced(message) => message,
        StopReason::Unavailable(unavailable) => format!(
            "seat is unavailable ({}): {}",
            unavailable.kind(),
            unavailable.message()
        ),
    };
    std::io::Error::other(message).into()
}

/// A frame clock that skips missed ticks: a stalled loop resumes on the
/// next beat instead of replaying the backlog in a burst.
fn frame_clock(period_ms: u64) -> tokio::time::Interval {
    let mut tick = tokio::time::interval(Duration::from_millis(period_ms));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick
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
