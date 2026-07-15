use std::fmt;
use std::sync::Arc;

/// Half-open byte range into an expression or template source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    pub start: usize,
    pub end: usize,
}

impl SourceSpan {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub const fn at(offset: usize) -> Self {
        Self {
            start: offset,
            end: offset,
        }
    }

    pub const fn join(self, other: Self) -> Self {
        Self {
            start: self.start,
            end: other.end,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPhase {
    Scan,
    Lex,
    Parse,
    Evaluate,
    Limit,
}

/// Field-aware authored diagnostic for expression compilation and evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionError {
    phase: ErrorPhase,
    field: Option<Arc<str>>,
    source: Arc<str>,
    span: SourceSpan,
    message: String,
    correction: Option<String>,
}

impl ExpressionError {
    pub(crate) fn new(
        phase: ErrorPhase,
        field: Option<Arc<str>>,
        source: Arc<str>,
        span: SourceSpan,
        message: impl Into<String>,
    ) -> Self {
        Self {
            phase,
            field,
            source,
            span,
            message: message.into(),
            correction: None,
        }
    }

    pub(crate) fn correction(mut self, correction: impl Into<String>) -> Self {
        self.correction = Some(correction.into());
        self
    }

    pub fn phase(&self) -> ErrorPhase {
        self.phase
    }

    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn span(&self) -> SourceSpan {
        self.span
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn correction_text(&self) -> Option<&str> {
        self.correction.as_deref()
    }

    pub fn line_column(&self) -> (usize, usize) {
        let offset = self.span.start.min(self.source.len());
        let prefix = &self.source[..offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let column = prefix
            .rsplit_once('\n')
            .map_or(prefix.chars().count() + 1, |(_, tail)| {
                tail.chars().count() + 1
            });
        (line, column)
    }

    fn source_excerpt(&self) -> Option<String> {
        if self.source.is_empty() {
            return None;
        }
        let mut offset = self.span.start.min(self.source.len());
        while !self.source.is_char_boundary(offset) {
            offset -= 1;
        }
        let line_start = self.source[..offset]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let line_end = self.source[offset..]
            .find('\n')
            .map_or(self.source.len(), |index| offset + index);
        let line = &self.source[line_start..line_end];
        const MAX_CHARS: usize = 160;
        let total = line.chars().count();
        if total <= MAX_CHARS {
            return Some(line.to_string());
        }

        let error_column = self.source[line_start..offset].chars().count();
        let window_start = error_column
            .saturating_sub(MAX_CHARS / 2)
            .min(total - MAX_CHARS);
        let mut excerpt = String::new();
        if window_start > 0 {
            excerpt.push('…');
        }
        excerpt.extend(line.chars().skip(window_start).take(MAX_CHARS));
        if window_start + MAX_CHARS < total {
            excerpt.push('…');
        }
        Some(excerpt)
    }
}

impl fmt::Display for ExpressionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(field) = &self.field {
            write!(formatter, "{field}: ")?;
        }
        let (line, column) = self.line_column();
        write!(
            formatter,
            "expression error at line {line}, column {column}: {}",
            self.message
        )?;
        if let Some(source) = self.source_excerpt() {
            write!(formatter, "; source {source:?}")?;
        }
        if let Some(correction) = &self.correction {
            write!(formatter, "; {correction}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ExpressionError {}
