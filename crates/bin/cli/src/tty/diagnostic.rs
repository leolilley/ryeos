use super::document::Hint;
use super::theme::Tone;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

impl DiagnosticLevel {
    pub fn tone(self) -> Tone {
        match self {
            Self::Info => Tone::Neutral,
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
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            level: DiagnosticLevel::Info,
            heading: None,
            message: message.into(),
            context: Vec::new(),
            hint: None,
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn informational_diagnostics_use_the_neutral_tone() {
        let diagnostic = Diagnostic::info("resolving surface");
        assert_eq!(diagnostic.level, DiagnosticLevel::Info);
        assert_eq!(diagnostic.level.tone(), Tone::Neutral);
    }
}
