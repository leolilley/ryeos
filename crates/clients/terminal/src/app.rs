//! App event loop — tokio select loop wiring crossterm input,
//! daemon events, tick, and rendering together.

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use futures_util::StreamExt;
use ryeos_tui_core::effects::Effect;
use ryeos_tui_core::input::{InputEvent, Key, Mouse, MouseAction, MouseButton, ScrollDirection};
use ryeos_tui_core::model::AppModel;
use ryeos_tui_core::update::{self, AppEvent};
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
    loaded_surface: ryeos_tui_core::surface::LoadedSurface,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut term = TerminalGuard::init()?;
    let _caps = RenderCapabilities::default();

    // Load scene config from scene.toml (CWD or ~/.config/ryeos/)
    let config = ryeos_tui_core::scene_config::SceneConfig::find();
    let tick_ms = config.animation.tick_ms;

    let (width, height) = term.size();
    let mut model = ryeos_tui_core::model::AppModel::from_surface(project_path, &loaded_surface);
    model.runtime.viewport = ryeos_tui_core::layout::Rect::new(0, 0, width, height);
    model.visual.animation.set_config(config);

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

    // Spawn crossterm event reader
    let events = EventStream::new();
    tokio::spawn(event_reader(events, input_tx, resize_tx));

    let mut renderer = FrameRenderer::new();
    let mut stdout = std::io::stdout();
    let mut tick_interval = tokio::time::interval(std::time::Duration::from_millis(tick_ms));
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
                run_effects(&mut model, &mut transport, &effects).await;
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
                let fake_thread_id = ryeos_tui_core::ids::ThreadId::new(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                );
                let thread_event = ryeos_tui_core::update::DaemonEvent::ThreadCreated {
                    id: fake_thread_id,
                    item_ref: Some(item_ref.clone()),
                };
                let start_event =
                    ryeos_tui_core::update::DaemonEvent::ThreadStarted { id: fake_thread_id };
                let user_part = ryeos_tui_core::update::DaemonEvent::TextDelta {
                    thread_id: fake_thread_id,
                    text: format!("{}\n", prompt),
                };
                ryeos_tui_core::update::update(
                    model,
                    ryeos_tui_core::update::AppEvent::DaemonBatch(vec![
                        thread_event,
                        start_event,
                        user_part,
                    ]),
                );

                // Focus the new thread in the thread tile
                for (tid, t) in model.workspace.tiles.iter_mut() {
                    if matches!(t.view, ryeos_tui_core::workspace::ViewSpec::Thread { .. }) {
                        t.view = ryeos_tui_core::workspace::ViewSpec::Thread {
                            thread_id: Some(fake_thread_id),
                        };
                        model.workspace.focused_tile = *tid;
                        break;
                    }
                }

                // Send to daemon
                let req = crate::transport::DaemonRequest::Execute {
                    item_ref: item_ref.clone(),
                    project_path: project_path.to_string_lossy().to_string(),
                    parameters: parameters.clone(),
                };
                match transport.request(req).await {
                    Ok(_resp) => {
                        // SSE streaming will be handled by the poll interval
                        // feeding daemon events back
                    }
                    Err(e) => {
                        let fail_event = ryeos_tui_core::update::DaemonEvent::ThreadFailed {
                            id: fake_thread_id,
                            error: format!("execute error: {}", e),
                        };
                        ryeos_tui_core::update::update(
                            model,
                            ryeos_tui_core::update::AppEvent::DaemonBatch(vec![fail_event]),
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
