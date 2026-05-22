//! Text utilities — word wrapping, truncation, formatting.
//!
//! Used by views to format content within tile bounds.

/// Word-wrap a string into lines of at most `max_width` characters.
/// Tries to break at word boundaries, falls back to character breaks.
pub fn word_wrap(text: &str, max_width: usize) -> Vec<&str> {
    if max_width == 0 {
        return vec![];
    }

    let mut lines = Vec::new();

    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push("");
            continue;
        }

        let mut remaining: &str = paragraph;
        while !remaining.is_empty() {
            if remaining.len() <= max_width {
                lines.push(remaining);
                break;
            }

            // Try to find a word boundary
            let mut split_at = max_width;
            for (i, ch) in remaining.char_indices() {
                if i > max_width {
                    break;
                }
                if ch == ' ' {
                    split_at = i;
                }
            }

            // If no space found, just split at max_width
            if split_at == 0 {
                split_at = max_width;
            }

            // Find actual char boundary
            let split_pos = remaining
                .char_indices()
                .nth(split_at)
                .map(|(i, _)| i)
                .unwrap_or(remaining.len());

            lines.push(remaining[..split_pos].trim_end());
            remaining = remaining[split_pos..].trim_start();
        }
    }

    lines
}

/// Truncate a string to fit within `max_width` characters, appending "…" if truncated.
pub fn truncate(text: &str, max_width: usize) -> String {
    if text.len() <= max_width {
        return text.to_string();
    }

    if max_width <= 1 {
        return "…".to_string();
    }

    let mut end = max_width - 1;
    // Find char boundary
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Format a duration in milliseconds to a human-readable string.
pub fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let mins = ms / 60_000;
        let secs = (ms % 60_000) / 1000;
        format!("{}m {}s", mins, secs)
    }
}

/// Format a timestamp (milliseconds since epoch) to a relative time string.
pub fn format_relative_time(timestamp_ms: i64, now_ms: i64) -> String {
    let diff = now_ms - timestamp_ms;
    if diff < 0 {
        return "just now".to_string();
    }

    let secs = diff / 1000;
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Format a cost in USD to a short string.
pub fn format_cost(usd: f64) -> String {
    if usd == 0.0 {
        "$0".to_string()
    } else if usd < 0.01 {
        format!("${:.4}", usd)
    } else {
        format!("${:.2}", usd)
    }
}

/// Format token count to short human-readable.
pub fn format_token_count(count: u64) -> String {
    if count < 1000 {
        format!("{}", count)
    } else if count < 1_000_000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_wrap_basic() {
        let lines = word_wrap("hello world foo bar", 11);
        assert_eq!(lines, vec!["hello world", "foo bar"]);
    }

    #[test]
    fn word_wrap_preserves_newlines() {
        let lines = word_wrap("hello\n\nworld", 80);
        assert_eq!(lines, vec!["hello", "", "world"]);
    }

    #[test]
    fn word_wrap_long_word() {
        let lines = word_wrap("abcdefghij", 5);
        assert_eq!(lines, vec!["abcde", "fghij"]);
    }

    #[test]
    fn truncate_short() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn truncate_exact() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long() {
        assert_eq!(truncate("hello world", 8), "hello w…");
    }

    #[test]
    fn format_duration() {
        assert_eq!(format_duration_ms(500), "500ms");
        assert_eq!(format_duration_ms(1500), "1.5s");
        assert_eq!(format_duration_ms(125000), "2m 5s");
    }

    #[test]
    fn format_relative_time_test() {
        assert_eq!(format_relative_time(999_000, 999_500), "0s ago");
        assert_eq!(format_relative_time(900_000, 960_000), "1m ago");
        assert_eq!(format_relative_time(0, 3600_000), "1h ago");
    }

    #[test]
    fn format_token_count_test() {
        assert_eq!(format_token_count(500), "500");
        assert_eq!(format_token_count(1500), "1.5k");
        assert_eq!(format_token_count(1_500_000), "1.5M");
    }
}
