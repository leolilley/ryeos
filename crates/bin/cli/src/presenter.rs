use std::io;

use serde_json::Value;

use crate::exec_stream::StreamOutcome;
use crate::transport::http::SseEvent;

pub enum Presenter {
    Plain,
    Tty(TtyPresenter),
}

impl Presenter {
    pub fn for_stdout(use_tty: bool) -> Self {
        if use_tty {
            Self::Tty(TtyPresenter::default())
        } else {
            Self::Plain
        }
    }

    pub fn loading(&mut self, command: &str, route: &str) -> io::Result<usize> {
        match self {
            Self::Plain => Ok(0),
            Self::Tty(tty) => tty.loading(command, route),
        }
    }

    pub fn structured_result(
        &mut self,
        command: &str,
        payload: &Value,
        previous_lines: usize,
    ) -> io::Result<bool> {
        match self {
            Self::Plain => Ok(false),
            Self::Tty(tty) => {
                tty.structured_result(command, payload, previous_lines)?;
                Ok(true)
            }
        }
    }

    pub fn stream_with_previous(
        &mut self,
        command: &str,
        previous_lines: usize,
    ) -> io::Result<()> {
        if let Self::Tty(tty) = self {
            tty.stream_with_previous(command, previous_lines)?;
        }
        Ok(())
    }

    pub fn stream_event(&mut self, ev: &SseEvent) -> io::Result<Option<StreamOutcome>> {
        match self {
            Self::Plain => Ok(None),
            Self::Tty(tty) => tty.stream_event(ev).map(Some),
        }
    }
}

#[derive(Default)]
pub struct TtyPresenter {
    stream: Option<crate::tty::TtyStreamPresenter>,
}

impl TtyPresenter {
    fn loading(&mut self, command: &str, route: &str) -> io::Result<usize> {
        crate::tty::render_command_loading(command, route)
    }

    fn structured_result(
        &mut self,
        command: &str,
        payload: &Value,
        previous_lines: usize,
    ) -> io::Result<usize> {
        crate::tty::render_command_result(command, payload, previous_lines)
    }

    fn stream_with_previous(&mut self, command: &str, previous_lines: usize) -> io::Result<()> {
        self.stream = Some(crate::tty::TtyStreamPresenter::with_previous(
            command,
            previous_lines,
        )?);
        Ok(())
    }

    fn stream_event(&mut self, ev: &SseEvent) -> io::Result<StreamOutcome> {
        match self.stream.as_mut() {
            Some(stream) => stream.render_event(ev),
            None => {
                let mut stream = crate::tty::TtyStreamPresenter::new("")?;
                let outcome = stream.render_event(ev)?;
                self.stream = Some(stream);
                Ok(outcome)
            }
        }
    }
}
