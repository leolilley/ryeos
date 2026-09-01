use sha2::{Digest, Sha256};
use std::collections::HashSet;

const MAX_RESULT_BYTES: usize = 100 * 1024;
const MIN_DEDUP_BYTES: usize = 1024;

pub struct GuardedResult {
    pub content: String,
    pub duplicate_of: Option<String>,
    pub truncated: bool,
}

pub struct GuardedBytes {
    pub bytes: Vec<u8>,
    pub duplicate_of: Option<String>,
    pub truncated: bool,
}

#[derive(Debug)]
pub struct ResultGuard {
    seen_hashes: HashSet<String>,
}

impl ResultGuard {
    pub fn new() -> Self {
        Self {
            seen_hashes: HashSet::new(),
        }
    }

    pub fn process(&mut self, content: &str) -> GuardedResult {
        let truncated = truncate(content, MAX_RESULT_BYTES);
        let was_truncated = truncated.len() != content.len();

        if truncated.len() < MIN_DEDUP_BYTES {
            return GuardedResult {
                content: truncated,
                duplicate_of: None,
                truncated: was_truncated,
            };
        }

        let hash = sha256_hex(&truncated);

        if self.seen_hashes.contains(&hash) {
            return GuardedResult {
                content: format!("[duplicate result omitted — hash {}]", &hash[..16]),
                duplicate_of: Some(hash),
                truncated: was_truncated,
            };
        }

        self.seen_hashes.insert(hash);
        GuardedResult {
            content: truncated,
            duplicate_of: None,
            truncated: was_truncated,
        }
    }

    pub fn process_bytes(&mut self, data: &[u8]) -> GuardedBytes {
        let content = String::from_utf8_lossy(data);
        let processed = self.process(&content);
        GuardedBytes {
            bytes: processed.content.into_bytes(),
            duplicate_of: processed.duplicate_of,
            truncated: processed.truncated,
        }
    }

    /// Rebuild the ordering-sensitive duplicate guard from one exact retained
    /// model-visible result. Recovery observes the already-guarded bytes, so
    /// hashing `content` reproduces the live key for both full and truncated
    /// first occurrences. A duplicate marker is accepted only after its exact
    /// hash has already appeared and only in the canonical live spelling.
    pub fn restore_result(
        &mut self,
        content: &str,
        duplicate_of: Option<&str>,
    ) -> Result<(), String> {
        if let Some(hash) = duplicate_of {
            if hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            {
                return Err("duplicate result hash is not canonical lowercase SHA-256".to_string());
            }
            if !self.seen_hashes.contains(hash) {
                return Err("duplicate result precedes its retained full result".to_string());
            }
            let expected = format!("[duplicate result omitted — hash {}]", &hash[..16]);
            if content != expected {
                return Err("duplicate result marker contradicts its retained hash".to_string());
            }
            return Ok(());
        }

        if content.len() >= MIN_DEDUP_BYTES {
            self.seen_hashes.insert(sha256_hex(content));
        }
        Ok(())
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
        assert_eq!(result.content, "hello world");
        assert!(result.duplicate_of.is_none());
        assert!(!result.truncated);
    }

    #[test]
    fn truncates_at_100kb() {
        let mut guard = ResultGuard::new();
        let big = "x".repeat(150_000);
        let result = guard.process(&big);
        assert!(result.content.len() < 110_000);
        assert!(result.content.ends_with("[...truncated at 100KB]"));
        assert!(result.truncated);
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
        assert!(result.content.len() < 110_000);
        assert!(String::from_utf8(result.content.clone().into_bytes()).is_ok());
    }

    #[test]
    fn small_identical_content_passes_through() {
        let mut guard = ResultGuard::new();
        let first = guard.process("same content");
        let second = guard.process("same content");
        assert_eq!(first.content, "same content");
        assert_eq!(second.content, "same content");
        assert!(second.duplicate_of.is_none());
    }

    #[test]
    fn deduplicates_large_identical_content() {
        let mut guard = ResultGuard::new();
        let content = "x".repeat(MIN_DEDUP_BYTES);
        let first = guard.process(&content);
        let second = guard.process(&content);
        assert_eq!(first.content, content);
        assert!(first.duplicate_of.is_none());
        assert!(second.content.contains("duplicate result omitted"));
        assert!(second.duplicate_of.is_some());
    }

    #[test]
    fn different_content_not_deduplicated() {
        let mut guard = ResultGuard::new();
        let first = guard.process("content A");
        let second = guard.process("content B");
        assert_eq!(first.content, "content A");
        assert_eq!(second.content, "content B");
    }

    #[test]
    fn process_bytes_works() {
        let mut guard = ResultGuard::new();
        let result = guard.process_bytes(b"test data");
        assert_eq!(String::from_utf8(result.bytes).unwrap(), "test data");
        assert!(result.duplicate_of.is_none());
        assert!(!result.truncated);
    }

    #[test]
    fn restored_result_preserves_duplicate_decision_across_restart() {
        let content = "x".repeat(MIN_DEDUP_BYTES);
        let hash = sha256_hex(&content);
        let mut guard = ResultGuard::new();
        guard.restore_result(&content, None).unwrap();

        let duplicate = guard.process(&content);
        assert_eq!(duplicate.duplicate_of.as_deref(), Some(hash.as_str()));
        assert_eq!(
            duplicate.content,
            format!("[duplicate result omitted — hash {}]", &hash[..16])
        );
    }

    #[test]
    fn restored_duplicate_requires_prior_full_result_and_exact_marker() {
        let content = "x".repeat(MIN_DEDUP_BYTES);
        let hash = sha256_hex(&content);
        let marker = format!("[duplicate result omitted — hash {}]", &hash[..16]);
        let mut guard = ResultGuard::new();
        assert!(guard.restore_result(&marker, Some(&hash)).is_err());
        guard.restore_result(&content, None).unwrap();
        assert!(guard.restore_result("wrong", Some(&hash)).is_err());
        guard.restore_result(&marker, Some(&hash)).unwrap();
    }
}
