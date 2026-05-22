//! Terminal capabilities detection — truecolor, unicode, animation.

/// Detected rendering capabilities.
#[derive(Debug, Clone)]
pub struct RenderCapabilities {
    pub truecolor: bool,
    pub unicode: bool,
    pub animation: bool,
}

impl Default for RenderCapabilities {
    fn default() -> Self {
        Self {
            truecolor: detect_truecolor(),
            unicode: true, // assume modern terminal
            animation: true,
        }
    }
}

fn detect_truecolor() -> bool {
    // Check COLORTERM for truecolor support
    if let Ok(ct) = std::env::var("COLORTERM") {
        matches!(ct.as_str(), "truecolor" | "24bit" | "yes")
    } else {
        false
    }
}
