//! App event loop — tokio select loop wiring crossterm input,
//! daemon events, tick, and rendering together.

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use futures_util::StreamExt;
use ryeos_client_base::effects::Effect;
use ryeos_client_base::input::{InputEvent, Key, Mouse, MouseAction, MouseButton, ScrollDirection};
use ryeos_client_base::model::AppModel;
use ryeos_client_base::update::{self, AppEvent};
use tokio::sync::mpsc;

use crate::bootstrap;
use crate::capabilities::RenderCapabilities;
use crate::render::FrameRenderer;
use crate::terminal::TerminalGuard;
use crate::transport::{DaemonTransport, MockTransport, SignedHttpTransport};

/// Run the TUI app.
pub async fn run(
    project_path: &str,
    mock: bool,
    loaded_surface: ryeos_client_base::surface::LoadedSurface,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut term = TerminalGuard::init()?;
    let _caps = RenderCapabilities::default();

    let (width, height) = term.size();
    let mut model = ryeos_client_base::model::AppModel::from_surface(project_path, &loaded_surface);
    model.runtime.viewport = ryeos_client_base::layout::Rect::new(0, 0, width, height);

    // Track surface file for hot reload (only for --surface-file)
    let surface_file_path: Option<std::path::PathBuf> = match &loaded_surface {
        ryeos_client_base::surface::LoadedSurface::LocalPreview { path, .. } => Some(path.clone()),
        _ => None,
    };
    let mut surface_file_mtime: Option<std::time::SystemTime> = surface_file_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok());

    // Create transport: try real daemon first, fall back to mock
    let mut transport: Box<dyn DaemonTransport> = if mock {
        Box::new(MockTransport)
    } else {
        match SignedHttpTransport::connect().await {
            Ok(t) => {
                tracing::info!("connected to daemon");
                Box::new(t)
            }
            Err(e) => {
                tracing::warn!("daemon not available ({}), using mock data", e);
                Box::new(MockTransport)
            }
        }
    };

    // Bootstrap
    let _bootstrap_result = bootstrap::blocking_essentials(&mut model, &mut transport).await;

    // Channels
    let (input_tx, mut input_rx) = mpsc::channel::<InputEvent>(256);
    let (resize_tx, mut resize_rx) = mpsc::channel::<(u16, u16)>(16);
    let (daemon_tx, mut daemon_rx) = mpsc::channel::<ryeos_client_base::update::DaemonEvent>(256);

    // Spawn crossterm event reader
    let events = EventStream::new();
    tokio::spawn(event_reader(events, input_tx, resize_tx));

    let mut renderer = FrameRenderer::new();
    let mut stdout = std::io::stdout();
    let mut tick_interval = tokio::time::interval(std::time::Duration::from_millis(100));
    let mut poll_interval = tokio::time::interval(std::time::Duration::from_secs(5));

    // Initial render
    if model.dirty {
        renderer.render(&mut stdout, &mut model, width, height)?;
        model.dirty = false;
    }

    loop {
        tokio::select! {
            Some(input) = input_rx.recv() => {
                let effects = update::update(&mut model, AppEvent::Input(input));
                if handle_effects(&effects) {
                    break;
                }
                run_effects(&mut model, &mut transport, &daemon_tx, &effects).await;
            }

            Some(event) = daemon_rx.recv() => {
                update::update(&mut model, AppEvent::Daemon(event));
            }

            Some((w, h)) = resize_rx.recv() => {
                let _ = term.update_size();
                update::update(&mut model, AppEvent::Resize { width: w, height: h });
            }

            _ = tick_interval.tick() => {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                update::update(&mut model, AppEvent::Tick { now_ms });
            }

            _ = poll_interval.tick() => {
                // Hot reload: check if surface file changed
                if let Some(ref path) = surface_file_path {
                    let new_mtime = std::fs::metadata(path)
                        .ok()
                        .and_then(|m| m.modified().ok());
                    let changed = match (surface_file_mtime, new_mtime) {
                        (Some(old), Some(new)) => new > old,
                        (None, Some(_)) => true,
                        _ => false,
                    };
                    if changed {
                        surface_file_mtime = new_mtime;
                        let opts = ryeos_client_base::surface::SurfaceLoadOptions {
                            explicit_file: Some(path.clone()),
                            surface_name: None,
                        };
                        let reloaded = ryeos_client_base::surface::load_surface(&opts);
                        let spec = reloaded.spec().clone();
                        for diag in reloaded.all_diagnostics() {
                            match diag {
                                ryeos_client_base::surface::SurfaceDiagnostic::ValidationError { message } => {
                                    tracing::warn!("surface reload: {}", message);
                                }
                                ryeos_client_base::surface::SurfaceDiagnostic::UnsupportedField { field, message } => {
                                    tracing::info!("surface reload: {}: {}", field, message);
                                }
                                ryeos_client_base::surface::SurfaceDiagnostic::Info { message } => {
                                    tracing::info!("surface reload: {}", message);
                                }
                            }
                        }
                        update::update(
                            &mut model,
                            AppEvent::SurfaceReloaded { spec },
                        );
                    }
                }

                match transport.poll_snapshot().await {
                    Ok(snapshot) => {
                        update::update(&mut model, AppEvent::PollSnapshot(snapshot));
                    }
                    Err(_) => {
                        // Daemon unreachable, keep running
                    }
                }
            }
        }

        // Render if dirty
        if model.dirty {
            let w = model.runtime.viewport.w;
            let h = model.runtime.viewport.h;
            renderer.render(&mut stdout, &mut model, w, h)?;
            model.dirty = false;
        }
    }

    Ok(())
}

/// Check if any effect is Quit.
fn handle_effects(effects: &[Effect]) -> bool {
    effects.iter().any(|e| matches!(e, Effect::Quit))
}

/// Execute effects from the core reducer.
async fn run_effects(
    model: &mut AppModel,
    transport: &mut Box<dyn DaemonTransport>,
    daemon_tx: &mpsc::Sender<ryeos_client_base::update::DaemonEvent>,
    effects: &[Effect],
) {
    for effect in effects {
        match effect {
            Effect::Execute {
                project_path,
                item_ref,
                parameters,
            } => {
                // Extract the prompt text
                let prompt = parameters
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or(item_ref);

                // Create a synthetic thread for the prompt
                let fake_thread_id = ryeos_client_base::ids::ThreadId::new(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                );
                let thread_event = ryeos_client_base::update::DaemonEvent::ThreadCreated {
                    id: fake_thread_id,
                    item_ref: Some(item_ref.clone()),
                };
                let start_event =
                    ryeos_client_base::update::DaemonEvent::ThreadStarted { id: fake_thread_id };
                let user_part = ryeos_client_base::update::DaemonEvent::TextDelta {
                    thread_id: fake_thread_id,
                    text: format!("{}\n", prompt),
                };
                ryeos_client_base::update::update(
                    model,
                    ryeos_client_base::update::AppEvent::DaemonBatch(vec![
                        thread_event,
                        start_event,
                        user_part,
                    ]),
                );

                // Focus the new thread in the thread tile
                for (tid, t) in model.workspace.tiles.iter_mut() {
                    if matches!(t.view, ryeos_client_base::workspace::ViewSpec::Thread { .. }) {
                        t.view = ryeos_client_base::workspace::ViewSpec::Thread {
                            thread_id: Some(fake_thread_id),
                        };
                        model.workspace.focused_tile = *tid;
                        break;
                    }
                }

                // Try SSE streaming first; fall back to non-streaming request
                let pp = project_path.to_string_lossy().to_string();
                let ir = item_ref.clone();
                let params = parameters.clone();
                match transport
                    .execute_stream(&ir, &pp, &params)
                    .await
                {
                    Ok(Some(mut stream)) => {
                        // Spawn a task that reads SSE events and feeds them
                        // through the daemon channel
                        let tx = daemon_tx.clone();
                        tokio::spawn(async move {
                            while let Some(sse_event) = stream.next_event().await {
                                if let Some(daemon_event) =
                                    sse_event.to_daemon_event(fake_thread_id)
                                {
                                    if tx.send(daemon_event).await.is_err() {
                                        break; // channel closed, app shutting down
                                    }
                                }
                            }
                        });
                    }
                    Ok(None) => {
                        // Transport doesn't support streaming (mock mode).
                        // The synthetic thread with user prompt is already visible.
                        // Generate a canned mock response.
                        let response_text =
                            "Mock response: item executed successfully.\n".to_string();
                        let resp_delta = ryeos_client_base::update::DaemonEvent::TextDelta {
                            thread_id: fake_thread_id,
                            text: response_text,
                        };
                        let complete =
                            ryeos_client_base::update::DaemonEvent::ThreadCompleted {
                                id: fake_thread_id,
                            };
                        ryeos_client_base::update::update(
                            model,
                            ryeos_client_base::update::AppEvent::DaemonBatch(vec![
                                resp_delta,
                                complete,
                            ]),
                        );
                    }
                    Err(e) => {
                        let fail_event = ryeos_client_base::update::DaemonEvent::ThreadFailed {
                            id: fake_thread_id,
                            error: format!("stream error: {}", e),
                        };
                        ryeos_client_base::update::update(
                            model,
                            ryeos_client_base::update::AppEvent::DaemonBatch(vec![fail_event]),
                        );
                    }
                }
            }
            Effect::RefreshState => {
                if let Ok(snapshot) = transport.poll_snapshot().await {
                    update::update(model, AppEvent::PollSnapshot(snapshot));
                }
            }
            Effect::SendThreadCommand {
                thread_id,
                command: _,
            } => {
                let req = crate::transport::DaemonRequest::CancelThread {
                    thread_id: *thread_id,
                };
                let _ = transport.request(req).await;
            }
            Effect::PersistSession => {
                crate::persistence::save_session(
                    &model.workspace.layout,
                    &model.workspace.tiles,
                    model.workspace.focused_tile,
                )
                .unwrap_or_else(|e| {
                    tracing::warn!("failed to save session: {e}");
                });
            }
            Effect::Quit => {}
        }
    }
}

/// Async event reader — converts crossterm events to core InputEvents.
async fn event_reader(
    events: EventStream,
    input_tx: mpsc::Sender<InputEvent>,
    resize_tx: mpsc::Sender<(u16, u16)>,
) {
    use futures_util::pin_mut;
    pin_mut!(events);

    while let Some(result) = events.next().await {
        match result {
            Ok(event) => match event {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    let input = convert_key(key);
                    let _ = input_tx.send(InputEvent::Key(input)).await;
                }
                Event::Mouse(mouse) => {
                    let input = convert_mouse(mouse);
                    let _ = input_tx.send(input).await;
                }
                Event::Resize(w, h) => {
                    let _ = resize_tx.send((w, h)).await;
                }
                _ => continue,
            },
            Err(_) => break,
        }
    }
}

fn convert_key(event: crossterm::event::KeyEvent) -> Key {
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    let alt = event.modifiers.contains(KeyModifiers::ALT);
    let shift = event.modifiers.contains(KeyModifiers::SHIFT);

    match event.code {
        KeyCode::Char(c) if ctrl => Key::Ctrl(c.to_ascii_lowercase()),
        KeyCode::Char(c) if alt => Key::Alt(c),
        KeyCode::Char(c) => Key::Char(c),

        KeyCode::Enter if shift || ctrl => Key::CtrlEnter,
        KeyCode::Enter if alt => Key::AltEnter,
        KeyCode::Enter => Key::Enter,

        KeyCode::Backspace if alt => Key::AltBackspace,
        KeyCode::Backspace => Key::Backspace,

        KeyCode::Tab if shift => Key::ShiftTab,
        KeyCode::Tab => Key::Tab,

        KeyCode::Esc => Key::Escape,

        KeyCode::Up => Key::ArrowUp,
        KeyCode::Down => Key::ArrowDown,
        KeyCode::Left => Key::ArrowLeft,
        KeyCode::Right => Key::ArrowRight,

        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,

        KeyCode::Delete => Key::Delete,
        KeyCode::F(n) => Key::F(n),

        _ => Key::Escape,
    }
}

fn convert_mouse(event: crossterm::event::MouseEvent) -> InputEvent {
    let action = match event.kind {
        MouseEventKind::Down(button) => {
            let btn = match button {
                crossterm::event::MouseButton::Left => MouseButton::Left,
                crossterm::event::MouseButton::Middle => MouseButton::Middle,
                crossterm::event::MouseButton::Right => MouseButton::Right,
            };
            MouseAction::Press(btn)
        }
        MouseEventKind::Up(button) => {
            let btn = match button {
                crossterm::event::MouseButton::Left => MouseButton::Left,
                crossterm::event::MouseButton::Middle => MouseButton::Middle,
                crossterm::event::MouseButton::Right => MouseButton::Right,
            };
            MouseAction::Release(btn)
        }
        MouseEventKind::ScrollUp => MouseAction::Scroll(ScrollDirection::Up),
        MouseEventKind::ScrollDown => MouseAction::Scroll(ScrollDirection::Down),
        _ => {
            return InputEvent::Mouse(Mouse {
                x: 0,
                y: 0,
                action: MouseAction::Scroll(ScrollDirection::Up),
            })
        }
    };

    InputEvent::Mouse(Mouse {
        x: event.column,
        y: event.row,
        action,
    })
}
