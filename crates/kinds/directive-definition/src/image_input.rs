//! Which models take images and in what form, declared as provider data.
//!
//! A provider route answers 200 and drops an image it cannot read, so support
//! cannot be learned from an error: an entry says what a model accepts, and a
//! model with no `accepts` block takes none. The request builder reads this and
//! carries no model or provider name.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a model without image support is given in place of an image.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum UnsupportedImagePolicy {
    /// The tool result is an error the model can read; nothing is stored.
    #[default]
    Refuse,
    /// The tool result keeps its text and the image becomes a note; nothing
    /// is stored.
    TextNote,
}

/// Where the images of a tool result travel in the request. Mechanical: each
/// value is one implementation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolResultImagePlacement {
    /// The tool message keeps its text and a note; one user message holding
    /// the images follows the run of tool messages.
    #[default]
    DeferredUserMessage,
}

/// How an image counts toward the estimated size of a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageCostConfig {
    #[serde(default)]
    pub base_tokens: u64,
    /// Pixels that make one token; positive.
    pub pixels_per_token: u64,
    /// No image counts more than this.
    pub max_tokens: u64,
    /// Tokens for an image whose dimensions are not recorded.
    pub unknown_dimensions_tokens: u64,
}

impl ImageCostConfig {
    /// Estimated tokens of one image: `base_tokens` plus the pixel count over
    /// `pixels_per_token`, rounded up and capped at `max_tokens`. An image
    /// with no recorded dimensions counts `unknown_dimensions_tokens`.
    pub fn tokens(&self, width: Option<u32>, height: Option<u32>) -> u64 {
        let (Some(width), Some(height)) = (width, height) else {
            return self.unknown_dimensions_tokens;
        };
        let pixels = u64::from(width).saturating_mul(u64::from(height));
        let per = self.pixels_per_token.max(1);
        let tokens = self
            .base_tokens
            .saturating_add(pixels / per + u64::from(pixels % per != 0));
        tokens.min(self.max_tokens)
    }
}

/// The form in which a model takes images.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageAcceptConfig {
    /// Exact `type/subtype` tokens the model reads.
    pub media_types: Vec<String>,
    pub max_bytes_per_image: u64,
    /// A request carries this many images at most, the newest; older ones
    /// become `omitted_note` text in place.
    pub max_images_per_request: u32,
    #[serde(default)]
    pub tool_result_placement: ToolResultImagePlacement,
    /// One text part of a content array. Whole-string placeholders: `{text}`.
    pub text_part_template: Value,
    /// One image part of a content array. Whole-string placeholders:
    /// `{data_url}`, `{data_b64}`, `{media_type}`, `{width}`, `{height}`.
    pub image_part_template: Value,
    /// Appended to a tool result whose images moved; named placeholder
    /// `{count}`.
    pub tool_result_note: String,
    /// Text part before each image of the moved message; `{index}` (one
    /// based) and `{call_id}`.
    pub deferred_label: String,
    /// Stands in for an image the request no longer carries; `{media_type}`,
    /// `{width}`, `{height}`, `{byte_length}`.
    pub omitted_note: String,
    pub cost: ImageCostConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageInputConfig {
    #[serde(default)]
    pub unsupported: UnsupportedImagePolicy,
    /// Absent: the model takes no images.
    #[serde(default)]
    pub accepts: Option<ImageAcceptConfig>,
}

const TEXT_PART_PLACEHOLDERS: &[&str] = &["text"];
const IMAGE_PART_PLACEHOLDERS: &[&str] = &["data_url", "data_b64", "media_type", "width", "height"];
const RESULT_NOTE_PLACEHOLDERS: &[&str] = &["count"];
const LABEL_PLACEHOLDERS: &[&str] = &["index", "call_id"];
const OMITTED_PLACEHOLDERS: &[&str] = &["media_type", "width", "height", "byte_length"];

/// Substitutes `{name}` placeholders in a note template. Validation has
/// already refused any other brace, so every `{` opens a known placeholder.
pub fn render_note(template: &str, values: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let name = &after[..close];
        match values.iter().find(|(key, _)| *key == name) {
            Some((_, value)) => out.push_str(value),
            None => {
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

fn validate_note(label: &str, template: &str, allowed: &[&str], context: &str) -> Result<()> {
    if template.is_empty() || template.len() > 512 || template.chars().any(char::is_control) {
        bail!(
            "provider config{context}: image_input.accepts.{label} must be 1-512 bytes without control characters"
        );
    }
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            bail!("provider config{context}: image_input.accepts.{label} has an unclosed brace");
        };
        let name = &after[..close];
        if !allowed.contains(&name) {
            bail!(
                "provider config{context}: image_input.accepts.{label} names unknown placeholder {{{name}}}; allowed: {allowed:?}"
            );
        }
        rest = &after[close + 1..];
    }
    if rest.contains('}') {
        bail!("provider config{context}: image_input.accepts.{label} has an unmatched brace");
    }
    Ok(())
}

/// Every whole-string placeholder of a part template must be one the builder
/// supplies; returns the placeholders found.
fn template_placeholders<'a>(
    label: &str,
    template: &'a Value,
    allowed: &[&str],
    context: &str,
) -> Result<Vec<&'a str>> {
    let mut found = Vec::new();
    let mut stack = vec![template];
    while let Some(value) = stack.pop() {
        match value {
            Value::String(text) => {
                if let Some(name) = text
                    .trim()
                    .strip_prefix('{')
                    .and_then(|rest| rest.strip_suffix('}'))
                {
                    if !allowed.contains(&name) {
                        bail!(
                            "provider config{context}: image_input.accepts.{label} names unknown placeholder {{{name}}}; allowed: {allowed:?}"
                        );
                    }
                    found.push(name);
                }
            }
            Value::Array(items) => stack.extend(items),
            Value::Object(map) => stack.extend(map.values()),
            _ => {}
        }
    }
    Ok(found)
}

fn valid_image_media_type(value: &str) -> bool {
    let Some(subtype) = value.strip_prefix("image/") else {
        return false;
    };
    !subtype.is_empty()
        && subtype.len() <= 64
        && subtype
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+-.".contains(c))
}

impl ImageInputConfig {
    pub fn validate(&self, context: &str) -> Result<()> {
        let Some(accepts) = &self.accepts else {
            return Ok(());
        };
        if accepts.media_types.is_empty() || accepts.media_types.len() > 16 {
            bail!(
                "provider config{context}: image_input.accepts.media_types must hold 1-16 entries"
            );
        }
        for media_type in &accepts.media_types {
            if !valid_image_media_type(media_type) {
                bail!(
                    "provider config{context}: image_input.accepts.media_types entry {media_type:?} is not an image/ subtype token"
                );
            }
        }
        if accepts.max_bytes_per_image == 0 {
            bail!(
                "provider config{context}: image_input.accepts.max_bytes_per_image must be positive"
            );
        }
        if accepts.max_images_per_request == 0 {
            bail!(
                "provider config{context}: image_input.accepts.max_images_per_request must be positive"
            );
        }
        if accepts.cost.pixels_per_token == 0 || accepts.cost.max_tokens == 0 {
            bail!(
                "provider config{context}: image_input.accepts.cost pixels_per_token and max_tokens must be positive"
            );
        }
        let text = template_placeholders(
            "text_part_template",
            &accepts.text_part_template,
            TEXT_PART_PLACEHOLDERS,
            context,
        )?;
        if !text.contains(&"text") {
            bail!(
                "provider config{context}: image_input.accepts.text_part_template must hold the {{text}} placeholder"
            );
        }
        let image = template_placeholders(
            "image_part_template",
            &accepts.image_part_template,
            IMAGE_PART_PLACEHOLDERS,
            context,
        )?;
        if !image.contains(&"data_url") && !image.contains(&"data_b64") {
            bail!(
                "provider config{context}: image_input.accepts.image_part_template must hold {{data_url}} or {{data_b64}}"
            );
        }
        validate_note(
            "tool_result_note",
            &accepts.tool_result_note,
            RESULT_NOTE_PLACEHOLDERS,
            context,
        )?;
        validate_note(
            "deferred_label",
            &accepts.deferred_label,
            LABEL_PLACEHOLDERS,
            context,
        )?;
        validate_note(
            "omitted_note",
            &accepts.omitted_note,
            OMITTED_PLACEHOLDERS,
            context,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn accepts() -> ImageAcceptConfig {
        serde_json::from_value(json!({
            "media_types": ["image/png"],
            "max_bytes_per_image": 1000,
            "max_images_per_request": 2,
            "text_part_template": {"type": "text", "text": "{text}"},
            "image_part_template": {"type": "image_url", "image_url": {"url": "{data_url}"}},
            "tool_result_note": "[{count} image(s) follow]",
            "deferred_label": "Image {index} from {call_id}:",
            "omitted_note": "[omitted {media_type} {width}x{height}]",
            "cost": {"pixels_per_token": 750, "max_tokens": 1600, "unknown_dimensions_tokens": 1600}
        }))
        .unwrap()
    }

    fn config(accepts: ImageAcceptConfig) -> ImageInputConfig {
        ImageInputConfig {
            unsupported: UnsupportedImagePolicy::Refuse,
            accepts: Some(accepts),
        }
    }

    #[test]
    fn a_complete_entry_validates_and_an_absent_accepts_takes_no_images() {
        config(accepts()).validate("").unwrap();
        let none: ImageInputConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(none.unsupported, UnsupportedImagePolicy::Refuse);
        assert!(none.accepts.is_none());
        none.validate("").unwrap();
    }

    #[test]
    fn unknown_placeholders_and_missing_required_ones_are_refused() {
        let mut bad = accepts();
        bad.deferred_label = "Image {nope}".to_string();
        assert!(
            config(bad)
                .validate("")
                .unwrap_err()
                .to_string()
                .contains("nope")
        );
        let mut bad = accepts();
        bad.omitted_note = "a { b".to_string();
        assert!(config(bad).validate("").is_err());
        let mut bad = accepts();
        bad.image_part_template = json!({"type": "image_url", "url": "{media_type}"});
        assert!(config(bad).validate("").is_err());
        let mut bad = accepts();
        bad.text_part_template = json!({"type": "text", "text": "{data_url}"});
        assert!(config(bad).validate("").is_err());
    }

    #[test]
    fn zero_limits_and_non_image_media_types_are_refused() {
        for mutate in [
            (|a: &mut ImageAcceptConfig| a.max_bytes_per_image = 0) as fn(&mut ImageAcceptConfig),
            |a| a.max_images_per_request = 0,
            |a| a.cost.pixels_per_token = 0,
            |a| a.media_types = vec![],
            |a| a.media_types = vec!["text/plain".to_string()],
        ] {
            let mut bad = accepts();
            mutate(&mut bad);
            assert!(config(bad).validate("").is_err());
        }
    }

    #[test]
    fn notes_render_named_placeholders() {
        assert_eq!(
            render_note(
                "Image {index} from {call_id}:",
                &[("index", "2".to_string()), ("call_id", "c9".to_string())]
            ),
            "Image 2 from c9:"
        );
        assert_eq!(render_note("no braces", &[]), "no braces");
    }

    #[test]
    fn cost_rounds_up_caps_and_falls_back_for_unknown_dimensions() {
        let cost = accepts().cost;
        assert_eq!(cost.tokens(Some(1), Some(1)), 1);
        assert_eq!(cost.tokens(Some(750), Some(2)), 2);
        assert_eq!(cost.tokens(Some(4000), Some(4000)), 1600);
        assert_eq!(cost.tokens(None, Some(10)), 1600);
        let with_base = ImageCostConfig {
            base_tokens: 85,
            ..cost
        };
        assert_eq!(with_base.tokens(Some(750), Some(1)), 86);
    }
}
