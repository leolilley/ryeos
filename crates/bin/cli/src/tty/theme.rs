#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Success,
    Active,
    Warning,
    Failure,
    Neutral,
    Secondary,
}

pub fn glyph(tone: Tone, unicode: bool) -> &'static str {
    match (tone, unicode) {
        (Tone::Success, true) => "◆",
        (Tone::Active, true) => "⠹",
        (Tone::Warning, true) => "▲",
        (Tone::Failure, true) => "✕",
        (Tone::Neutral | Tone::Secondary, true) => "•",
        (Tone::Success, false) => "OK",
        (Tone::Active, false) => "..",
        (Tone::Warning, false) => "WARN",
        (Tone::Failure, false) => "ERROR",
        (Tone::Neutral | Tone::Secondary, false) => "-",
    }
}

pub fn style(value: &str, tone: Tone, color: bool) -> String {
    if !color {
        return value.to_string();
    }
    let code = match tone {
        // Keep these semantic tones aligned with the Gruvbox tokens in the UI.
        Tone::Success => "1;38;2;142;192;124",
        Tone::Active => "38;2;214;93;14",
        Tone::Warning => "1;38;2;250;189;47",
        Tone::Failure => "1;38;2;251;73;52",
        Tone::Neutral => "38;2;213;196;161",
        Tone::Secondary => "38;2;168;153;132",
    };
    format!("\x1b[{code}m{value}\x1b[0m")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_tones_use_the_ui_gruvbox_tokens() {
        assert!(style("x", Tone::Active, true).contains("38;2;214;93;14"));
        assert!(style("x", Tone::Success, true).contains("38;2;142;192;124"));
        assert!(style("x", Tone::Warning, true).contains("38;2;250;189;47"));
        assert!(style("x", Tone::Failure, true).contains("38;2;251;73;52"));
        assert!(style("x", Tone::Neutral, true).contains("38;2;213;196;161"));
        assert!(style("x", Tone::Secondary, true).contains("38;2;168;153;132"));
    }
}
