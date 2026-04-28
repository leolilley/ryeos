//! Token budget estimation.

/// Estimate token count using chars/4 with 10% safety reserve.
pub fn estimate_tokens(text: &str) -> usize {
    // chars/4 with 10% safety reserve, floor of 1
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
}
