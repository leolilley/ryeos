use std::io::{self, Write};

use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{execute, queue};

/// Incrementally repaints only the lines emitted by the active command flow.
pub(crate) struct Frame<W: Write> {
    output: W,
    rendered_lines: u16,
}

impl Frame<io::Stdout> {
    pub(crate) fn stdout() -> Self {
        Self::new(io::stdout())
    }
}

impl<W: Write> Frame<W> {
    pub(crate) const fn new(output: W) -> Self {
        Self {
            output,
            rendered_lines: 0,
        }
    }

    pub(crate) fn render(&mut self, lines: &[String]) -> io::Result<()> {
        let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
        let line_width = usize::from(width.saturating_sub(1)).max(1);
        let row_limit = usize::from(height.saturating_sub(1)).max(1);
        let bounded = if lines.len() <= row_limit {
            lines
                .iter()
                .map(|line| super::super::clamp_visible(line, line_width))
                .collect::<Vec<_>>()
        } else {
            let mut visible = lines
                .iter()
                .take(row_limit.saturating_sub(1))
                .map(|line| super::super::clamp_visible(line, line_width))
                .collect::<Vec<_>>();
            visible.push(super::super::clamp_visible(
                lines.last().expect("non-empty clipped frame"),
                line_width,
            ));
            visible
        };
        self.erase()?;
        for line in &bounded {
            queue!(self.output, MoveToColumn(0))?;
            write!(self.output, "{line}\r\n")?;
        }
        self.rendered_lines = u16::try_from(bounded.len()).unwrap_or(u16::MAX);
        self.output.flush()
    }

    pub(crate) fn clear(&mut self) -> io::Result<()> {
        self.erase()?;
        self.output.flush()
    }

    #[cfg(test)]
    pub(crate) fn into_inner(self) -> W {
        self.output
    }

    fn erase(&mut self) -> io::Result<()> {
        if self.rendered_lines > 0 {
            execute!(
                self.output,
                MoveUp(self.rendered_lines),
                MoveToColumn(0),
                Clear(ClearType::FromCursorDown)
            )?;
            self.rendered_lines = 0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repaint_clears_only_the_previous_command_viewport() {
        let mut frame = Frame::new(Vec::new());
        frame.render(&["one".into(), "two".into()]).unwrap();
        frame.render(&["three".into()]).unwrap();
        let bytes = frame.into_inner();
        let rendered = String::from_utf8(bytes).unwrap();
        assert!(rendered.starts_with("\u{1b}[1Gone\r\n\u{1b}[1Gtwo\r\n"));
        assert!(rendered.contains("\u{1b}[2A\u{1b}[1G\u{1b}[0J"));
        assert!(rendered.ends_with("\u{1b}[1Gthree\r\n"));
    }

    #[test]
    fn clearing_an_empty_frame_emits_nothing() {
        let mut frame = Frame::new(Vec::new());
        frame.clear().unwrap();
        assert!(frame.into_inner().is_empty());
    }

    #[test]
    fn viewport_bounds_are_applied_before_repaint_accounting() {
        let mut frame = Frame::new(Vec::new());
        frame
            .render(&["x".repeat(200), "middle".into(), "footer".into()])
            .unwrap();
        let rendered = String::from_utf8(frame.into_inner()).unwrap();
        assert!(!rendered.contains(&"x".repeat(200)));
        assert!(rendered.contains("footer"));
    }
}
