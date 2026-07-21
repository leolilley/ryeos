use std::io;
use std::time::Duration;

use crossterm::event::{Event as CrosstermEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Modifiers {
    pub(crate) control: bool,
    pub(crate) alt: bool,
    pub(crate) shift: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Escape,
    Backspace,
    Delete,
    Tab,
    BackTab,
    Char(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyEvent {
    pub(crate) key: Key,
    pub(crate) modifiers: Modifiers,
}

impl KeyEvent {
    #[cfg(test)]
    pub(crate) const fn plain(key: Key) -> Self {
        Self {
            key,
            modifiers: Modifiers {
                control: false,
                alt: false,
                shift: false,
            },
        }
    }

    pub(crate) fn is_control(self, value: char) -> bool {
        self.modifiers.control
            && matches!(self.key, Key::Char(actual) if actual.eq_ignore_ascii_case(&value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    Key(KeyEvent),
    Paste(String),
    Resize { width: u16, height: u16 },
    Tick,
    Terminate,
}

pub(crate) struct EventReader {
    input: EventStream,
    ticks: tokio::time::Interval,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
}

impl EventReader {
    pub(crate) fn new(tick_interval: Duration) -> io::Result<Self> {
        let mut ticks = tokio::time::interval(tick_interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        #[cfg(unix)]
        let terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        #[cfg(unix)]
        let hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        #[cfg(unix)]
        let interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        Ok(Self {
            input: EventStream::new(),
            ticks,
            #[cfg(unix)]
            terminate,
            #[cfg(unix)]
            hangup,
            #[cfg(unix)]
            interrupt,
        })
    }

    #[cfg(unix)]
    pub(crate) async fn next(&mut self) -> io::Result<Event> {
        loop {
            tokio::select! {
                _ = self.ticks.tick() => return Ok(Event::Tick),
                _ = self.terminate.recv() => return Ok(Event::Terminate),
                _ = self.hangup.recv() => return Ok(Event::Terminate),
                _ = self.interrupt.recv() => return Ok(Event::Terminate),
                event = self.input.next() => {
                    let event = event.transpose()?.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::UnexpectedEof, "terminal event stream closed")
                    })?;
                    if let Some(event) = translate(event) {
                        return Ok(event);
                    }
                }
            }
        }
    }

    #[cfg(not(unix))]
    pub(crate) async fn next(&mut self) -> io::Result<Event> {
        loop {
            tokio::select! {
                _ = self.ticks.tick() => return Ok(Event::Tick),
                event = self.input.next() => {
                    let event = event.transpose()?.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::UnexpectedEof, "terminal event stream closed")
                    })?;
                    if let Some(event) = translate(event) {
                        return Ok(event);
                    }
                }
            }
        }
    }
}

fn translate(event: CrosstermEvent) -> Option<Event> {
    match event {
        CrosstermEvent::Key(event)
            if matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
        {
            let key = match event.code {
                KeyCode::Up => Key::Up,
                KeyCode::Down => Key::Down,
                KeyCode::Left => Key::Left,
                KeyCode::Right => Key::Right,
                KeyCode::PageUp => Key::PageUp,
                KeyCode::PageDown => Key::PageDown,
                KeyCode::Home => Key::Home,
                KeyCode::End => Key::End,
                KeyCode::Enter => Key::Enter,
                KeyCode::Esc => Key::Escape,
                KeyCode::Backspace => Key::Backspace,
                KeyCode::Delete => Key::Delete,
                KeyCode::Tab => Key::Tab,
                KeyCode::BackTab => Key::BackTab,
                KeyCode::Char(value) => Key::Char(value),
                _ => return None,
            };
            Some(Event::Key(KeyEvent {
                key,
                modifiers: Modifiers {
                    control: event.modifiers.contains(KeyModifiers::CONTROL),
                    alt: event.modifiers.contains(KeyModifiers::ALT),
                    shift: event.modifiers.contains(KeyModifiers::SHIFT),
                },
            }))
        }
        CrosstermEvent::Paste(value) => Some(Event::Paste(value)),
        CrosstermEvent::Resize(width, height) => Some(Event::Resize { width, height }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent as CrosstermKeyEvent, KeyEventState};

    #[test]
    fn translates_navigation_and_modifiers() {
        let translated = translate(CrosstermEvent::Key(CrosstermKeyEvent {
            code: KeyCode::Char('d'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }));
        assert_eq!(
            translated,
            Some(Event::Key(KeyEvent {
                key: Key::Char('d'),
                modifiers: Modifiers {
                    control: true,
                    alt: false,
                    shift: false,
                },
            }))
        );
    }

    #[test]
    fn ignores_key_release_events() {
        let translated = translate(CrosstermEvent::Key(CrosstermKeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        }));
        assert_eq!(translated, None);
    }
}
