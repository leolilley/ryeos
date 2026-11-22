use crate::directive::ToolOutputConfig;
use ryeos_runtime::callback::EventAttachment;
use ryeos_runtime::{TOOL_RESULT_PARTIAL_PREFIX, TOOL_RESULT_SPILLED, ToolResultSpill};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

const MIN_DEDUP_BYTES: usize = 1024;

/// The media type a spilled result is stored under.
pub const SPILL_MEDIA_TYPE: &str = "text/plain";

/// `truncated_reason` of a result cut by the size ceiling.
pub const HARD_CUT_REASON: &str = "result_guard";

/// Whether the guard may store an oversize result. A node that cannot store it
/// is learned only from its refusal of the append, so the runner re-renders the
/// result with `Disabled` and the reason the record then carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpillMode {
    Allowed,
    Disabled(&'static str),
}

/// A stored full result and what the record says about it.
#[derive(Debug, Clone)]
pub struct GuardedSpill {
    pub upload: EventAttachment,
    pub record: ToolResultSpill,
}

pub struct GuardedResult {
    pub content: String,
    pub duplicate_of: Option<String>,
    pub truncated: bool,
    /// Why the text was cut, when it was.
    pub truncated_reason: Option<&'static str>,
    pub spill: Option<GuardedSpill>,
    /// The duplicate-guard key this call added, so a re-render can take it
    /// back with [`ResultGuard::forget`].
    pub key: Option<String>,
}

pub struct GuardedBytes {
    pub bytes: Vec<u8>,
    pub duplicate_of: Option<String>,
    pub truncated: bool,
    pub truncated_reason: Option<&'static str>,
    pub spill: Option<GuardedSpill>,
    pub key: Option<String>,
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

    /// Take back the key a call added: its result was rendered again.
    pub fn forget(&mut self, key: &str) {
        self.seen_hashes.remove(key);
    }

    pub fn process(
        &mut self,
        content: &str,
        config: &ToolOutputConfig,
        mode: SpillMode,
    ) -> GuardedResult {
        let (text, truncated_reason, spill) = bound(content, config, mode);
        let truncated = truncated_reason.is_some();

        if text.len() < MIN_DEDUP_BYTES {
            return GuardedResult {
                content: text,
                duplicate_of: None,
                truncated,
                truncated_reason,
                spill,
                key: None,
            };
        }

        let hash = sha256_hex(&text);

        if self.seen_hashes.contains(&hash) {
            // The identical result was shown, and stored when it spilled,
            // earlier in this run: nothing is stored again.
            return GuardedResult {
                content: format!("[duplicate result omitted — hash {}]", &hash[..16]),
                duplicate_of: Some(hash),
                truncated,
                truncated_reason: truncated.then_some(HARD_CUT_REASON),
                spill: None,
                key: None,
            };
        }

        self.seen_hashes.insert(hash.clone());
        GuardedResult {
            content: text,
            duplicate_of: None,
            truncated,
            truncated_reason,
            spill,
            key: Some(hash),
        }
    }

    pub fn process_bytes(
        &mut self,
        data: &[u8],
        config: &ToolOutputConfig,
        mode: SpillMode,
    ) -> GuardedBytes {
        let content = String::from_utf8_lossy(data);
        let processed = self.process(&content, config, mode);
        GuardedBytes {
            bytes: processed.content.into_bytes(),
            duplicate_of: processed.duplicate_of,
            truncated: processed.truncated,
            truncated_reason: processed.truncated_reason,
            spill: processed.spill,
            key: processed.key,
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

/// The model-visible text for `content`, why it was cut, and the stored copy
/// when there is one. A result within the threshold is returned whole.
fn bound(
    content: &str,
    config: &ToolOutputConfig,
    mode: SpillMode,
) -> (String, Option<&'static str>, Option<GuardedSpill>) {
    let threshold = config.spill_threshold_bytes as usize;
    if content.len() <= threshold {
        return (content.to_string(), None, None);
    }
    match mode {
        SpillMode::Allowed => match spill(content, config) {
            Some((text, spilled)) => (text, Some(TOOL_RESULT_SPILLED), Some(spilled)),
            // The text could not be named as an attachment: keep the old cut.
            None => cut(content, config.hard_cut_bytes as usize, HARD_CUT_REASON),
        },
        SpillMode::Disabled(reason) => cut(content, config.hard_cut_bytes as usize, reason),
    }
}

fn cut(
    content: &str,
    max_bytes: usize,
    reason: &'static str,
) -> (String, Option<&'static str>, Option<GuardedSpill>) {
    if content.len() <= max_bytes {
        return (content.to_string(), None, None);
    }
    let mut text = content[..floor_boundary(content, max_bytes)].to_string();
    text.push_str(&format!("\n[...truncated at {}KB]", max_bytes / 1024));
    (text, Some(reason), None)
}

fn spill(content: &str, config: &ToolOutputConfig) -> Option<(String, GuardedSpill)> {
    let stored_end = floor_boundary(content, config.max_stored_bytes as usize);
    let stored = &content[..stored_end];
    let upload = EventAttachment::new(SPILL_MEDIA_TYPE, stored.as_bytes().to_vec()).ok()?;
    let head_end = floor_boundary(content, config.preview_head_bytes as usize);
    let tail_start = ceil_boundary(
        content,
        content
            .len()
            .saturating_sub(config.preview_tail_bytes as usize),
    )
    .max(head_end);
    let total = content.len();
    let omitted = tail_start - head_end;
    let mut text = format!(
        "{TOOL_RESULT_PARTIAL_PREFIX}{total} bytes, full result stored as attachment {}; the full result is not available to you]\n",
        upload.object_hash()
    );
    if stored_end < total {
        text.push_str(&format!(
            "[only the first {stored_end} of {total} bytes were stored]\n"
        ));
    }
    text.push_str(&format!("[first {head_end} bytes]\n"));
    text.push_str(&content[..head_end]);
    text.push_str(&format!("\n[... {omitted} bytes omitted ...]\n"));
    text.push_str(&format!("[last {} bytes]\n", total - tail_start));
    text.push_str(&content[tail_start..]);
    let record = ToolResultSpill {
        attachment: upload.object_hash().to_string(),
        stored_bytes: stored_end as u64,
        total_bytes: total as u64,
        preview_head_bytes: head_end as u64,
        preview_tail_bytes: (total - tail_start) as u64,
    };
    Some((text, GuardedSpill { upload, record }))
}

fn floor_boundary(s: &str, mut index: usize) -> usize {
    index = index.min(s.len());
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(s: &str, mut index: usize) -> usize {
    index = index.min(s.len());
    while index < s.len() && !s.is_char_boundary(index) {
        index += 1;
    }
    index
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
        let result = guard.process(
            "hello world",
            &ToolOutputConfig::default(),
            SpillMode::Disabled(HARD_CUT_REASON),
        );
        assert_eq!(result.content, "hello world");
        assert!(result.duplicate_of.is_none());
        assert!(!result.truncated);
    }

    #[test]
    fn truncates_at_100kb() {
        let mut guard = ResultGuard::new();
        let big = "x".repeat(150_000);
        let result = guard.process(
            &big,
            &ToolOutputConfig::default(),
            SpillMode::Disabled(HARD_CUT_REASON),
        );
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
        let result = guard.process(
            &content,
            &ToolOutputConfig::default(),
            SpillMode::Disabled(HARD_CUT_REASON),
        );
        assert!(result.content.len() < 110_000);
        assert!(String::from_utf8(result.content.clone().into_bytes()).is_ok());
    }

    #[test]
    fn small_identical_content_passes_through() {
        let mut guard = ResultGuard::new();
        let first = guard.process(
            "same content",
            &ToolOutputConfig::default(),
            SpillMode::Disabled(HARD_CUT_REASON),
        );
        let second = guard.process(
            "same content",
            &ToolOutputConfig::default(),
            SpillMode::Disabled(HARD_CUT_REASON),
        );
        assert_eq!(first.content, "same content");
        assert_eq!(second.content, "same content");
        assert!(second.duplicate_of.is_none());
    }

    #[test]
    fn deduplicates_large_identical_content() {
        let mut guard = ResultGuard::new();
        let content = "x".repeat(MIN_DEDUP_BYTES);
        let first = guard.process(
            &content,
            &ToolOutputConfig::default(),
            SpillMode::Disabled(HARD_CUT_REASON),
        );
        let second = guard.process(
            &content,
            &ToolOutputConfig::default(),
            SpillMode::Disabled(HARD_CUT_REASON),
        );
        assert_eq!(first.content, content);
        assert!(first.duplicate_of.is_none());
        assert!(second.content.contains("duplicate result omitted"));
        assert!(second.duplicate_of.is_some());
    }

    #[test]
    fn different_content_not_deduplicated() {
        let mut guard = ResultGuard::new();
        let first = guard.process(
            "content A",
            &ToolOutputConfig::default(),
            SpillMode::Disabled(HARD_CUT_REASON),
        );
        let second = guard.process(
            "content B",
            &ToolOutputConfig::default(),
            SpillMode::Disabled(HARD_CUT_REASON),
        );
        assert_eq!(first.content, "content A");
        assert_eq!(second.content, "content B");
    }

    #[test]
    fn process_bytes_works() {
        let mut guard = ResultGuard::new();
        let result = guard.process_bytes(
            b"test data",
            &ToolOutputConfig::default(),
            SpillMode::Disabled(HARD_CUT_REASON),
        );
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

        let duplicate = guard.process(
            &content,
            &ToolOutputConfig::default(),
            SpillMode::Disabled(HARD_CUT_REASON),
        );
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

    fn small_config() -> ToolOutputConfig {
        ToolOutputConfig {
            spill_threshold_bytes: 4000,
            preview_head_bytes: 1500,
            preview_tail_bytes: 500,
            max_stored_bytes: 10000,
            hard_cut_bytes: 6000,
        }
    }

    #[test]
    fn oversize_result_spills_with_a_pinned_partial_header() {
        let mut guard = ResultGuard::new();
        let body: String = (0..8000)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let result = guard.process(&body, &small_config(), SpillMode::Allowed);
        let spill = result.spill.expect("an oversize result spills");
        assert_eq!(result.truncated_reason, Some("result_spilled"));
        assert!(result.truncated);
        assert_eq!(spill.record.total_bytes, 8000);
        assert_eq!(spill.record.stored_bytes, 8000);
        assert_eq!(spill.record.preview_head_bytes, 1500);
        assert_eq!(spill.record.preview_tail_bytes, 500);
        assert_eq!(spill.upload.bytes(), body.as_bytes());
        assert_eq!(spill.upload.media_type(), "text/plain");
        let header = format!(
            "[tool result partial: 8000 bytes, full result stored as attachment {}; the full result is not available to you]",
            spill.upload.object_hash()
        );
        assert_eq!(result.content.lines().next().unwrap(), header);
        assert!(result.content.contains("[first 1500 bytes]"));
        assert!(result.content.contains("[... 6000 bytes omitted ...]"));
        assert!(result.content.contains("[last 500 bytes]"));
        assert!(result.content.ends_with(&body[7500..]));
        assert!(result.content.len() < 2500);
    }

    #[test]
    fn result_within_the_threshold_is_untouched() {
        let mut guard = ResultGuard::new();
        let body = "y".repeat(4000);
        let result = guard.process(&body, &small_config(), SpillMode::Allowed);
        assert_eq!(result.content, body);
        assert!(!result.truncated && result.spill.is_none());
    }

    #[test]
    fn result_beyond_the_store_limit_says_how_much_was_stored() {
        let mut guard = ResultGuard::new();
        let body = "z".repeat(15000);
        let result = guard.process(&body, &small_config(), SpillMode::Allowed);
        let spill = result.spill.unwrap();
        assert_eq!(spill.record.stored_bytes, 10000);
        assert_eq!(spill.record.total_bytes, 15000);
        assert!(
            result
                .content
                .contains("[only the first 10000 of 15000 bytes were stored]")
        );
    }

    #[test]
    fn disabled_spill_cuts_and_names_its_cause() {
        let mut guard = ResultGuard::new();
        let body = "q".repeat(8000);
        let result = guard.process(
            &body,
            &small_config(),
            SpillMode::Disabled("spill_media_type_not_admitted"),
        );
        assert_eq!(
            result.truncated_reason,
            Some("spill_media_type_not_admitted")
        );
        assert!(result.spill.is_none());
        assert!(result.content.ends_with("[...truncated at 5KB]"));
    }

    #[test]
    fn a_repeated_spilled_result_collapses_and_stores_nothing_again() {
        let mut guard = ResultGuard::new();
        let body = "r".repeat(8000);
        let first = guard.process(&body, &small_config(), SpillMode::Allowed);
        assert!(first.spill.is_some());
        let second = guard.process(&body, &small_config(), SpillMode::Allowed);
        assert!(second.duplicate_of.is_some());
        assert!(second.spill.is_none());
    }

    #[test]
    fn forgetting_a_render_lets_the_cut_text_be_first_again() {
        let mut guard = ResultGuard::new();
        let body = "w".repeat(8000);
        let first = guard.process(&body, &small_config(), SpillMode::Allowed);
        guard.forget(first.key.as_deref().unwrap());
        let again = guard.process(&body, &small_config(), SpillMode::Disabled("x"));
        assert!(again.duplicate_of.is_none());
    }

    #[test]
    fn the_preview_of_multibyte_text_stays_on_char_boundaries() {
        let mut guard = ResultGuard::new();
        let body = "日".repeat(3000);
        let result = guard.process(&body, &small_config(), SpillMode::Allowed);
        assert!(result.spill.is_some());
        assert!(String::from_utf8(result.content.into_bytes()).is_ok());
    }

    #[test]
    fn a_restored_preview_keeps_the_live_duplicate_key() {
        let mut live = ResultGuard::new();
        let body = "k".repeat(8000);
        let first = live.process(&body, &small_config(), SpillMode::Allowed);
        let mut restored = ResultGuard::new();
        restored.restore_result(&first.content, None).unwrap();
        let again = restored.process(&body, &small_config(), SpillMode::Allowed);
        assert_eq!(again.duplicate_of, first.key);
    }
}
