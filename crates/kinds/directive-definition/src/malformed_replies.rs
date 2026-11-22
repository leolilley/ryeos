//! Malformed replies, declared as provider data.
//!
//! Some models write a tool call as plain text in their own markup instead
//! of issuing a structured call. An entry lists the exact strings that mark
//! such text, the message that corrects the model, and how many corrections
//! in a row are allowed. The runtime reads opaque strings: it carries no
//! protocol name and no meaning for any marker, and the message is operator
//! text.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Most markers an entry can declare.
pub const MALFORMED_REPLIES_MAX_MARKERS: usize = 16;
/// Longest marker, in bytes.
pub const MALFORMED_REPLIES_MAX_MARKER_BYTES: usize = 128;
/// Longest corrective message, in bytes.
pub const MALFORMED_REPLIES_MAX_MESSAGE_BYTES: usize = 4096;
/// Most corrections in a row an entry can allow.
pub const MALFORMED_REPLIES_MAX_CONSECUTIVE: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MalformedRepliesPolicy {
    /// A reply whose text contains any of these strings is malformed. Plain,
    /// case-sensitive substring match.
    pub markers: Vec<String>,
    /// The message sent to the model in place of the malformed reply.
    pub message: String,
    /// How many malformed replies in a row are corrected before the run
    /// fails.
    pub max_consecutive: u32,
}

impl MalformedRepliesPolicy {
    /// The first listed marker that occurs in `text`.
    pub fn first_marker_in(&self, text: &str) -> Option<&str> {
        self.markers
            .iter()
            .map(String::as_str)
            .find(|marker| text.contains(marker))
    }

    pub fn validate(&self, context: &str) -> Result<()> {
        if self.markers.is_empty() || self.markers.len() > MALFORMED_REPLIES_MAX_MARKERS {
            bail!(
                "provider config{context}: malformed_replies.markers must hold 1-{MALFORMED_REPLIES_MAX_MARKERS} entries"
            );
        }
        for (index, marker) in self.markers.iter().enumerate() {
            if marker.is_empty()
                || marker.len() > MALFORMED_REPLIES_MAX_MARKER_BYTES
                || marker.chars().any(char::is_control)
            {
                bail!(
                    "provider config{context}: malformed_replies.markers entries must be 1-{MALFORMED_REPLIES_MAX_MARKER_BYTES} bytes without control characters"
                );
            }
            for other in &self.markers[..index] {
                if other == marker {
                    bail!(
                        "provider config{context}: malformed_replies.markers lists {marker:?} twice"
                    );
                }
                if other.contains(marker.as_str()) || marker.contains(other.as_str()) {
                    bail!(
                        "provider config{context}: malformed_replies.markers entry {marker:?} overlaps {other:?}; one is a substring of the other"
                    );
                }
            }
        }
        if self.message.trim().is_empty()
            || self.message.len() > MALFORMED_REPLIES_MAX_MESSAGE_BYTES
            || self.message.chars().any(char::is_control)
        {
            bail!(
                "provider config{context}: malformed_replies.message must be 1-{MALFORMED_REPLIES_MAX_MESSAGE_BYTES} bytes without control characters"
            );
        }
        if self.max_consecutive == 0 || self.max_consecutive > MALFORMED_REPLIES_MAX_CONSECUTIVE {
            bail!(
                "provider config{context}: malformed_replies.max_consecutive must be 1-{MALFORMED_REPLIES_MAX_CONSECUTIVE}"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> MalformedRepliesPolicy {
        MalformedRepliesPolicy {
            markers: vec!["<|channel|>".to_string(), "<|message|>".to_string()],
            message: "Reply with a structured tool call.".to_string(),
            max_consecutive: 3,
        }
    }

    #[test]
    fn a_policy_parses_and_validates() {
        let parsed: MalformedRepliesPolicy = serde_json::from_value(serde_json::json!({
            "markers": ["<|channel|>"],
            "message": "again",
            "max_consecutive": 2,
        }))
        .unwrap();
        parsed.validate("").unwrap();
        policy().validate("").unwrap();
    }

    #[test]
    fn every_field_is_required_and_unknown_fields_are_refused() {
        for missing in ["markers", "message", "max_consecutive"] {
            let mut value = serde_json::json!({
                "markers": ["x"], "message": "m", "max_consecutive": 1,
            });
            value.as_object_mut().unwrap().remove(missing);
            assert!(
                serde_json::from_value::<MalformedRepliesPolicy>(value).is_err(),
                "{missing}"
            );
        }
        let extra = serde_json::json!({
            "markers": ["x"], "message": "m", "max_consecutive": 1, "other": 1,
        });
        assert!(serde_json::from_value::<MalformedRepliesPolicy>(extra).is_err());
    }

    #[test]
    fn marker_bounds_are_enforced() {
        let mut p = policy();
        p.markers = vec![];
        assert!(p.validate("").is_err());
        p.markers = (0..17).map(|i| format!("m{i:02}")).collect();
        assert!(p.validate("").is_err());
        p.markers = (0..16).map(|i| format!("m{i:02}")).collect();
        assert!(p.validate("").is_ok());
        p.markers = vec![String::new()];
        assert!(p.validate("").is_err());
        p.markers = vec!["a".repeat(129)];
        assert!(p.validate("").is_err());
        p.markers = vec!["a".repeat(128)];
        assert!(p.validate("").is_ok());
        p.markers = vec!["a\nb".to_string()];
        assert!(p.validate("").is_err());
    }

    #[test]
    fn duplicate_and_overlapping_markers_are_refused() {
        let mut p = policy();
        p.markers = vec!["<|a|>".to_string(), "<|a|>".to_string()];
        assert!(p.validate("").is_err());
        p.markers = vec!["<|a|>".to_string(), "x<|a|>y".to_string()];
        assert!(p.validate("").is_err());
        p.markers = vec!["x<|a|>y".to_string(), "<|a|>".to_string()];
        assert!(p.validate("").is_err());
    }

    #[test]
    fn message_bounds_are_enforced() {
        let mut p = policy();
        p.message = String::new();
        assert!(p.validate("").is_err());
        p.message = "a".repeat(4097);
        assert!(p.validate("").is_err());
        p.message = "a".repeat(4096);
        assert!(p.validate("").is_ok());
        p.message = "a\u{7}b".to_string();
        assert!(p.validate("").is_err());
    }

    #[test]
    fn max_consecutive_bounds_are_enforced() {
        let mut p = policy();
        p.max_consecutive = 0;
        assert!(p.validate("").is_err());
        p.max_consecutive = 9;
        assert!(p.validate("").is_err());
        p.max_consecutive = 8;
        assert!(p.validate("").is_ok());
        p.max_consecutive = 1;
        assert!(p.validate("").is_ok());
    }

    #[test]
    fn first_marker_is_a_case_sensitive_plain_substring() {
        let p = policy();
        assert_eq!(
            p.first_marker_in("a <|message|> b <|channel|>"),
            Some("<|channel|>")
        );
        assert_eq!(p.first_marker_in("a <|MESSAGE|> b"), None);
        assert_eq!(p.first_marker_in("channel message"), None);
        assert_eq!(p.first_marker_in(""), None);
    }
}
