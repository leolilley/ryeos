use sha2::{Digest, Sha256};
use std::collections::HashSet;

const MAX_RESULT_BYTES: usize = 100 * 1024;

pub struct ResultGuard {
    seen_hashes: HashSet<String>,
}

impl ResultGuard {
    pub fn new() -> Self {
        Self {
            seen_hashes: HashSet::new(),
        }
    }

    pub fn process(&mut self, content: &str) -> String {
        let truncated = truncate(content, MAX_RESULT_BYTES);
        let hash = sha256_hex(&truncated);

        if self.seen_hashes.contains(&hash) {
            return format!(
                "[duplicate result omitted — hash {}]",
                &hash[..16]
            );
        }

        self.seen_hashes.insert(hash);
        truncated
    }

    pub fn process_bytes(&mut self, data: &[u8]) -> Vec<u8> {
        let content = String::from_utf8_lossy(data);
        let processed = self.process(&content);
        processed.into_bytes()
    }
}

fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }

    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }

    let mut truncated = s[..end].to_string();
    truncated.push_str("\n[...truncated at 100KB]");
    truncated
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_content_passes_through() {
        let mut guard = ResultGuard::new();
        let result = guard.process("hello world");
        assert_eq!(result, "hello world");
    }

    #[test]
    fn truncates_at_100kb() {
        let mut guard = ResultGuard::new();
        let big = "x".repeat(150_000);
        let result = guard.process(&big);
        assert!(result.len() < 110_000);
        assert!(result.ends_with("[...truncated at 100KB]"));
    }

    #[test]
    fn truncates_at_char_boundary() {
        let mut guard = ResultGuard::new();
        let mut content = String::new();
        let byte_target = 100 * 1024;
        while content.len() < byte_target + 10 {
            content.push('日');
        }
        let result = guard.process(&content);
        assert!(result.len() < 110_000);
        assert!(String::from_utf8(result.clone().into_bytes()).is_ok());
    }

    #[test]
    fn deduplicates_identical_content() {
        let mut guard = ResultGuard::new();
        let first = guard.process("same content");
        let second = guard.process("same content");
        assert_eq!(first, "same content");
        assert!(second.contains("duplicate result omitted"));
    }

    #[test]
    fn different_content_not_deduplicated() {
        let mut guard = ResultGuard::new();
        let first = guard.process("content A");
        let second = guard.process("content B");
        assert_eq!(first, "content A");
        assert_eq!(second, "content B");
    }

    #[test]
    fn process_bytes_works() {
        let mut guard = ResultGuard::new();
        let result = guard.process_bytes(b"test data");
        assert_eq!(String::from_utf8(result).unwrap(), "test data");
    }
}
