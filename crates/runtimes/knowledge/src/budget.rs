//! Token budget estimation.

/// Estimate token count using the default knowledge heuristic.
pub fn estimate_tokens(text: &str) -> usize {
    // Exact default behavior: chars/4 with 10% safety reserve, integer
    // floor, minimum 1. Counts Unicode scalar values, not bytes.
    let raw = text.chars().count();
    if raw == 0 {
        return 1;
    }
    ((raw * 11) / 40).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_returns_one() {
        assert_eq!(estimate_tokens(""), 1);
    }

    #[test]
    fn single_char_returns_one() {
        assert_eq!(estimate_tokens("a"), 1);
    }

    #[test]
    fn monotonic_in_length() {
        let a = estimate_tokens("hello");
        let b = estimate_tokens("hello world");
        let c = estimate_tokens("hello world this is a longer string");
        assert!(a <= b);
        assert!(b <= c);
    }

    #[test]
    fn known_value() {
        // 40 chars → 40 * 11 / 40 = 11 tokens
        let text = "a".repeat(40);
        assert_eq!(estimate_tokens(&text), 11);
    }

    #[test]
    fn boundary_values_use_integer_floor() {
        assert_eq!(estimate_tokens(&"a".repeat(39)), 10);
        assert_eq!(estimate_tokens(&"a".repeat(40)), 11);
        assert_eq!(estimate_tokens(&"a".repeat(41)), 11);
    }

    #[test]
    fn unicode_counts_chars_not_bytes() {
        let text = "🦀".repeat(40);
        assert_eq!(text.len(), 160);
        assert_eq!(text.chars().count(), 40);
        assert_eq!(estimate_tokens(&text), 11);
    }
}
