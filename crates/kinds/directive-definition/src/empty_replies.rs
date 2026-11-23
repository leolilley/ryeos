//! Empty replies, declared as provider data.
//!
//! Some models end a turn with no tool call and no text: a reply that holds
//! only reasoning, or only whitespace. An entry can declare the message that
//! corrects the model and how many corrections in a row are allowed. The
//! trigger is structural (nothing to act on, a normal stop finish), so there
//! is no marker list. The message is operator text.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Longest corrective message, in bytes.
pub const EMPTY_REPLIES_MAX_MESSAGE_BYTES: usize = 4096;
/// Most corrections in a row an entry can allow.
pub const EMPTY_REPLIES_MAX_CONSECUTIVE: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyRepliesPolicy {
    /// How many empty replies in a row are corrected before the run fails.
    pub max_consecutive: u32,
    /// The message sent to the model in place of the empty reply.
    pub message: String,
}

impl EmptyRepliesPolicy {
    pub fn validate(&self, context: &str) -> Result<()> {
        if self.max_consecutive == 0 || self.max_consecutive > EMPTY_REPLIES_MAX_CONSECUTIVE {
            bail!(
                "provider config{context}: empty_replies.max_consecutive must be 1-{EMPTY_REPLIES_MAX_CONSECUTIVE}"
            );
        }
        if self.message.trim().is_empty()
            || self.message.len() > EMPTY_REPLIES_MAX_MESSAGE_BYTES
            || self.message.chars().any(char::is_control)
        {
            bail!(
                "provider config{context}: empty_replies.message must be 1-{EMPTY_REPLIES_MAX_MESSAGE_BYTES} bytes without control characters"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> EmptyRepliesPolicy {
        EmptyRepliesPolicy {
            max_consecutive: 2,
            message: "Call a tool or give your answer.".to_string(),
        }
    }

    #[test]
    fn a_policy_parses_and_validates() {
        let parsed: EmptyRepliesPolicy = serde_json::from_value(serde_json::json!({
            "max_consecutive": 2,
            "message": "again",
        }))
        .unwrap();
        parsed.validate("").unwrap();
        policy().validate("").unwrap();
    }

    #[test]
    fn every_field_is_required_and_unknown_fields_are_refused() {
        for missing in ["message", "max_consecutive"] {
            let mut value = serde_json::json!({"message": "m", "max_consecutive": 1});
            value.as_object_mut().unwrap().remove(missing);
            assert!(
                serde_json::from_value::<EmptyRepliesPolicy>(value).is_err(),
                "{missing}"
            );
        }
        let extra = serde_json::json!({"message": "m", "max_consecutive": 1, "other": 1});
        assert!(serde_json::from_value::<EmptyRepliesPolicy>(extra).is_err());
    }

    #[test]
    fn message_bounds_are_enforced() {
        let mut p = policy();
        p.message = String::new();
        assert!(p.validate("").is_err());
        p.message = "   ".to_string();
        assert!(p.validate("").is_err());
        p.message = "a".repeat(4097);
        assert!(p.validate("").is_err());
        p.message = "a".repeat(4096);
        assert!(p.validate("").is_ok());
        p.message = "a\u{7}b".to_string();
        assert!(p.validate("").is_err());
    }

    #[test]
    fn max_consecutive_bounds_name_the_setting() {
        let mut p = policy();
        p.max_consecutive = 0;
        let zero = p.validate("").unwrap_err().to_string();
        assert!(zero.contains("empty_replies.max_consecutive"), "{zero}");
        p.max_consecutive = 9;
        let over = p.validate("").unwrap_err().to_string();
        assert!(over.contains("empty_replies.max_consecutive"), "{over}");
        p.max_consecutive = 8;
        assert!(p.validate("").is_ok());
        p.max_consecutive = 1;
        assert!(p.validate("").is_ok());
    }
}
