//! Request-side cache marks, declared as provider data.
//!
//! Some routes cache a prompt prefix only where the request marks it. An entry
//! says how a mark is written, how a plain string becomes a part that can hold
//! one, how many marks the route allows, and which messages carry them. The
//! request builder reads this and carries no model or provider name.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which message of the final request list carries a mark. Mechanical: each
/// value is one selector over the wire messages, not a policy choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CacheMarkPosition {
    /// The first message with role `system`.
    System,
    /// The last message of the request.
    Newest,
    /// The message just before the last assistant message: the bytes the
    /// previous request marked as its newest.
    PreviousTail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheMarksConfig {
    /// Object keys merged into the last content part of a marked message.
    pub mark: Value,
    /// How a string content becomes a part; the whole-string placeholder
    /// `{text}` takes the string.
    pub text_part_template: Value,
    /// The route's limit on marks in one request.
    pub max_marks: u32,
    /// Priority order; entries past `max_marks` are dropped.
    pub positions: Vec<CacheMarkPosition>,
}

/// Most marks a route can be declared to allow.
pub const CACHE_MARKS_MAX: u32 = 4;

fn holds_text_placeholder(value: &Value) -> bool {
    match value {
        Value::String(text) => text.trim() == "{text}",
        Value::Array(items) => items.iter().any(holds_text_placeholder),
        Value::Object(map) => map.values().any(holds_text_placeholder),
        _ => false,
    }
}

fn unknown_placeholder(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let name = text
                .trim()
                .strip_prefix('{')
                .and_then(|rest| rest.strip_suffix('}'))?;
            (name != "text").then(|| name.to_string())
        }
        Value::Array(items) => items.iter().find_map(unknown_placeholder),
        Value::Object(map) => map.values().find_map(unknown_placeholder),
        _ => None,
    }
}

impl CacheMarksConfig {
    pub fn validate(&self, context: &str) -> Result<()> {
        match &self.mark {
            Value::Object(map) if !map.is_empty() => {}
            _ => bail!("provider config{context}: cache_marks.mark must be a non-empty object"),
        }
        if !self.text_part_template.is_object() {
            bail!("provider config{context}: cache_marks.text_part_template must be an object");
        }
        if let Some(name) = unknown_placeholder(&self.text_part_template) {
            bail!(
                "provider config{context}: cache_marks.text_part_template names unknown placeholder {{{name}}}; allowed: [\"text\"]"
            );
        }
        if !holds_text_placeholder(&self.text_part_template) {
            bail!(
                "provider config{context}: cache_marks.text_part_template must hold the {{text}} placeholder"
            );
        }
        if self.max_marks == 0 || self.max_marks > CACHE_MARKS_MAX {
            bail!("provider config{context}: cache_marks.max_marks must be 1 to {CACHE_MARKS_MAX}");
        }
        if self.positions.is_empty() {
            bail!("provider config{context}: cache_marks.positions must not be empty");
        }
        let mut seen = std::collections::HashSet::new();
        for position in &self.positions {
            if !seen.insert(*position) {
                bail!("provider config{context}: cache_marks.positions lists {position:?} twice");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> CacheMarksConfig {
        serde_json::from_value(json!({
            "mark": {"cache_control": {"type": "ephemeral"}},
            "text_part_template": {"type": "text", "text": "{text}"},
            "max_marks": 4,
            "positions": ["system", "newest", "previous_tail"]
        }))
        .unwrap()
    }

    #[test]
    fn a_complete_entry_validates() {
        config().validate("").unwrap();
    }

    #[test]
    fn an_unknown_key_or_position_is_refused_at_parse() {
        assert!(
            serde_json::from_value::<CacheMarksConfig>(json!({
                "mark": {"k": 1},
                "text_part_template": {"text": "{text}"},
                "max_marks": 1,
                "positions": ["newest"],
                "extra": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<CacheMarksConfig>(json!({
                "mark": {"k": 1},
                "text_part_template": {"text": "{text}"},
                "max_marks": 1,
                "positions": ["oldest"]
            }))
            .is_err()
        );
    }

    #[test]
    fn bad_values_are_refused() {
        let refused = |edit: &dyn Fn(&mut CacheMarksConfig)| {
            let mut bad = config();
            edit(&mut bad);
            bad.validate("").unwrap_err().to_string()
        };
        assert!(refused(&|c| c.mark = json!({})).contains("mark"));
        assert!(refused(&|c| c.mark = json!("x")).contains("mark"));
        assert!(refused(&|c| c.text_part_template = json!({"type": "text"})).contains("{text}"));
        assert!(refused(&|c| c.text_part_template = json!({"text": "{nope}"})).contains("nope"));
        assert!(refused(&|c| c.max_marks = 0).contains("max_marks"));
        assert!(refused(&|c| c.max_marks = 5).contains("max_marks"));
        assert!(refused(&|c| c.positions = vec![]).contains("positions"));
        assert!(
            refused(&|c| c.positions = vec![CacheMarkPosition::Newest, CacheMarkPosition::Newest])
                .contains("twice")
        );
    }
}
