use std::io;

use serde_json::Value;

use crate::exec_stream::StreamOutcome;
use crate::transport::http::SseEvent;

pub enum Presenter {
    Plain(crate::tty::Console),
    Tty(Box<TtyPresenter>),
}

pub enum StructuredPresentation {
    Machine,
    Rendered,
    Failed(String),
}

impl Presenter {
    pub fn for_console(console: &crate::tty::Console) -> Self {
        if console.capabilities().tty() {
            Self::Tty(Box::new(TtyPresenter::new(console.clone())))
        } else {
            Self::Plain(console.clone())
        }
    }

    pub fn loading(&mut self, command: &str, route: &str) -> io::Result<usize> {
        match self {
            Self::Plain(_) => Ok(0),
            Self::Tty(tty) => tty.loading(command, route),
        }
    }

    pub fn structured_result(
        &mut self,
        command: &str,
        payload: &Value,
        previous_lines: usize,
    ) -> io::Result<StructuredPresentation> {
        match self {
            Self::Plain(_) => Ok(StructuredPresentation::Machine),
            Self::Tty(tty) => {
                if let Some(detail) = crate::tty::structured_result_failure(payload) {
                    tty.clear_loading()?;
                    return Ok(StructuredPresentation::Failed(detail));
                }
                tty.structured_result(command, payload, previous_lines)?;
                Ok(StructuredPresentation::Rendered)
            }
        }
    }

    pub fn stream_with_previous(&mut self, command: &str, previous_lines: usize) -> io::Result<()> {
        if let Self::Tty(tty) = self {
            tty.stream_with_previous(command, previous_lines)?;
        }
        Ok(())
    }

    pub fn stream_event(&mut self, ev: &SseEvent) -> io::Result<StreamOutcome> {
        match self {
            Self::Plain(console) => crate::exec_stream::render_event(console, ev),
            Self::Tty(tty) => tty.stream_event(ev),
        }
    }
}

pub struct TtyPresenter {
    console: crate::tty::Console,
    loading: Option<crate::tty::OperationProgress>,
    stream: Option<crate::tty::TtyStreamPresenter>,
}

impl TtyPresenter {
    fn new(console: crate::tty::Console) -> Self {
        Self {
            console,
            loading: None,
            stream: None,
        }
    }

    fn loading(&mut self, command: &str, route: &str) -> io::Result<usize> {
        let mut progress = self
            .console
            .progress(crate::tty::OperationKind::Run, command)?;
        if let Some(progress) = progress.as_mut() {
            progress.update(command, Some(route))?;
        }
        self.loading = progress;
        Ok(0)
    }

    fn structured_result(
        &mut self,
        command: &str,
        payload: &Value,
        previous_lines: usize,
    ) -> io::Result<usize> {
        self.clear_loading()?;
        crate::tty::render_command_result(&self.console, command, payload, previous_lines)
    }

    fn stream_with_previous(&mut self, command: &str, previous_lines: usize) -> io::Result<()> {
        self.clear_loading()?;
        self.stream = Some(crate::tty::TtyStreamPresenter::with_previous(
            self.console.clone(),
            command,
            previous_lines,
        )?);
        Ok(())
    }

    fn clear_loading(&mut self) -> io::Result<()> {
        if let Some(progress) = self.loading.take() {
            progress.finish()?;
        }
        Ok(())
    }

    fn stream_event(&mut self, ev: &SseEvent) -> io::Result<StreamOutcome> {
        match self.stream.as_mut() {
            Some(stream) => stream.render_event(ev),
            None => {
                let mut stream = crate::tty::TtyStreamPresenter::new(self.console.clone(), "")?;
                let outcome = stream.render_event(ev)?;
                self.stream = Some(stream);
                Ok(outcome)
            }
        }
    }
}
