use super::document::Hint;
use super::theme::Tone;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Warning,
    Error,
}

impl DiagnosticLevel {
    pub fn tone(self) -> Tone {
        match self {
            Self::Warning => Tone::Warning,
            Self::Error => Tone::Failure,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub heading: Option<String>,
    pub message: String,
    pub context: Vec<String>,
    pub hint: Option<Hint>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: DiagnosticLevel::Error,
            heading: None,
            message: message.into(),
            context: Vec::new(),
            hint: None,
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            level: DiagnosticLevel::Warning,
            heading: None,
            message: message.into(),
            context: Vec::new(),
            hint: None,
        }
    }
}
