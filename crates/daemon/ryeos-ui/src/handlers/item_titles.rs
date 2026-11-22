//! Display names for listed items, taken from a signed UI configuration.
//!
//! The configuration says, per kind, which top-level document keys name an
//! item and whether a markdown heading counts. This module implements three
//! mechanical sources (an authored key, the first heading, the leaf of the
//! bare id) and carries no kind names. A name read from an item is shown only
//! when the item's effective trust class is trusted; every other item gets
//! the derived leaf. The configuration is resolved with no project in scope,
//! so a project cannot supply its own.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_engine::contracts::SignatureEnvelope;

use super::signed_item_topology::TrustSummary;

pub const CONFIG_REF: &str = "config:ryeos/ui/item-titles";
const CONFIG_PATH: &str = "ryeos/ui/item-titles.yaml";

pub const SOURCE_AUTHORED: &str = "authored";
pub const SOURCE_HEADING: &str = "heading";
pub const SOURCE_DERIVED: &str = "derived";

/// How one kind's name is taken from its item.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TitleRule {
    /// Top-level document keys, tried in order.
    #[serde(default)]
    pub authored: Vec<String>,
    /// Whether the first level-one markdown heading counts.
    #[serde(default)]
    pub heading: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TitleLimits {
    pub heading_scan_bytes: usize,
    pub max_chars: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemTitleConfig {
    // Header fields every signed config carries; unused here.
    #[allow(dead_code)]
    category: String,
    #[allow(dead_code)]
    version: String,
    #[allow(dead_code)]
    description: String,
    pub kinds: BTreeMap<String, TitleRule>,
    pub default: TitleRule,
    pub authored_requires_trust: bool,
    pub limits: TitleLimits,
}

impl ItemTitleConfig {
    pub fn from_value(value: Value) -> Result<Self, String> {
        let config: Self = serde_json::from_value(value).map_err(|e| e.to_string())?;
        if config.limits.heading_scan_bytes == 0 || config.limits.max_chars == 0 {
            return Err("item-titles limits must be positive".into());
        }
        for rule in config
            .kinds
            .values()
            .chain(std::iter::once(&config.default))
        {
            if rule.authored.iter().any(|key| key.trim().is_empty()) {
                return Err("item-titles authored keys must be non-empty".into());
            }
        }
        Ok(config)
    }

    /// Resolve and verify the signed config; missing or untrusted refuses.
    pub fn load(state: &AppState) -> Result<Self> {
        let value = super::ui_attention::load_signed_config(state, CONFIG_PATH, CONFIG_REF)?;
        Ok(Self::from_value(value)
            .map_err(|e| HandlerError::Internal(format!("{CONFIG_REF}: {e}")))?)
    }

    fn rule(&self, kind: &str) -> &TitleRule {
        self.kinds.get(kind).unwrap_or(&self.default)
    }

    /// The name and its source for one item. `trusted` is the item's
    /// effective trust class already decided by the caller. `read` returns the
    /// item's whole source and is called only when a name may be read from it;
    /// `parse` parses that source with the kind's parser, and is called only
    /// when an authored key occurs in the text. A read or parse failure yields
    /// the derived name.
    pub fn title(
        &self,
        kind: &str,
        bare_id: &str,
        trusted: bool,
        envelope: Option<&SignatureEnvelope>,
        read: impl FnOnce() -> Option<String>,
        parse: impl FnOnce(&str) -> Option<Value>,
    ) -> (String, &'static str) {
        let rule = self.rule(kind);
        let readable = (trusted || !self.authored_requires_trust)
            && (!rule.authored.is_empty() || rule.heading);
        if readable && let Some(source) = read() {
            // A parse can launch the kind's parser, so it runs only when one of
            // the keys appears literally in the text. A key spelled only with
            // escapes is missed, and the item shows its derived name.
            if rule
                .authored
                .iter()
                .any(|key| source.contains(key.as_str()))
                && let Some(Value::Object(document)) = parse(&source)
            {
                for key in &rule.authored {
                    if let Some(Value::String(text)) = document.get(key)
                        && let Some(title) = sanitise(text, self.limits.max_chars)
                    {
                        return (title, SOURCE_AUTHORED);
                    }
                }
            }
            if rule.heading
                && let Some(title) = self.heading(&source, envelope)
            {
                return (title, SOURCE_HEADING);
            }
        }
        (derive(bare_id), SOURCE_DERIVED)
    }

    fn heading(&self, source: &str, envelope: Option<&SignatureEnvelope>) -> Option<String> {
        let stripped;
        let source = match envelope {
            Some(envelope) => {
                stripped = lillux::signature::strip_signature_lines_with_envelope(
                    source,
                    &envelope.prefix,
                    envelope.suffix.as_deref(),
                );
                stripped.as_str()
            }
            None => source,
        };
        let body = after_frontmatter(source);
        let mut end = body.len().min(self.limits.heading_scan_bytes);
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        let mut fenced = false;
        for line in body[..end].lines() {
            if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
                fenced = !fenced;
                continue;
            }
            if !fenced
                && let Some(text) = line.strip_prefix("# ")
                && let Some(title) = sanitise(text, self.limits.max_chars)
            {
                return Some(title);
            }
        }
        None
    }
}

/// The text after a leading `---` frontmatter block, or all of it when there
/// is none.
fn after_frontmatter(source: &str) -> &str {
    let Some(rest) = source
        .strip_prefix("---\n")
        .or_else(|| source.strip_prefix("---\r\n"))
    else {
        return source;
    };
    let mut offset = 0;
    for segment in rest.split_inclusive('\n') {
        offset += segment.len();
        if segment.trim_end() == "---" {
            return &rest[offset..];
        }
    }
    source
}

/// Control characters and invisible formatting characters (bidirectional
/// overrides and isolates, zero-width characters, the byte-order mark)
/// dropped, so a title cannot reorder or hide what it shows; whitespace runs
/// collapsed to one space, ends trimmed, and the result cut to `max_chars`
/// characters. `None` when nothing is left.
fn sanitise(text: &str, max_chars: usize) -> Option<String> {
    let spaced: String = text
        .chars()
        .filter_map(|c| {
            if is_invisible_format(c) {
                None
            } else if c.is_whitespace() {
                Some(' ')
            } else if c.is_control() {
                None
            } else {
                Some(c)
            }
        })
        .collect();
    let collapsed = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = collapsed.chars().take(max_chars).collect();
    let capped = capped.trim_end().to_string();
    (!capped.is_empty()).then_some(capped)
}

/// Formatting characters that change how text is ordered or shown without
/// being visible themselves.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
    )
}

/// Whether an item's trust class, as the list already computed it, is the
/// trusted class. An item whose source could not be classified is not.
pub fn is_trusted(trust: Option<&TrustSummary>) -> bool {
    trust.is_some_and(|trust| trust.class == "trusted")
}

/// The leaf of the bare id with `-` and `_` as spaces.
fn derive(bare_id: &str) -> String {
    super::signed_item_topology::label_for_bare_id(bare_id).replace(['-', '_'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shipped() -> ItemTitleConfig {
        let path = ryeos_engine::test_support::workspace_root()
            .join("bundles/ryeos-ui/.ai/config/ryeos/ui/item-titles.yaml");
        let text = std::fs::read_to_string(path).unwrap();
        ItemTitleConfig::from_value(serde_yaml::from_str(&text).unwrap()).unwrap()
    }

    fn yaml_envelope() -> SignatureEnvelope {
        SignatureEnvelope {
            prefix: "#".into(),
            suffix: None,
            after_shebang: false,
        }
    }

    fn markdown_envelope() -> SignatureEnvelope {
        SignatureEnvelope {
            prefix: "<!--".into(),
            suffix: Some("-->".into()),
            after_shebang: false,
        }
    }

    fn yaml_title(
        config: &ItemTitleConfig,
        kind: &str,
        trusted: bool,
        text: &str,
    ) -> (String, &'static str) {
        config.title(
            kind,
            "ryeos/ui/my-fancy_item",
            trusted,
            Some(&yaml_envelope()),
            || Some(text.to_string()),
            |s| serde_yaml::from_str::<Value>(s).ok(),
        )
    }

    #[test]
    fn shipped_config_parses() {
        let config = shipped();
        assert!(config.authored_requires_trust);
        assert!(config.kinds["directive"].heading);
        assert!(!config.default.heading);
    }

    #[test]
    fn parser_is_not_launched_when_no_authored_key_occurs() {
        let config = shipped();
        let mut parsed = false;
        let (title, source) = config.title(
            "graph",
            "ryeos/ui/my-fancy_item",
            true,
            Some(&yaml_envelope()),
            || Some("name: x\ndescription: nothing here\n".to_string()),
            |_| {
                parsed = true;
                None
            },
        );
        assert!(!parsed);
        assert_eq!((title.as_str(), source), ("my fancy item", SOURCE_DERIVED));
    }

    #[test]
    fn trusted_bundle_item_shows_its_authored_title() {
        let config = shipped();
        let (title, source) = yaml_title(&config, "graph", true, "title: Nightly build\nname: x\n");
        assert_eq!((title.as_str(), source), ("Nightly build", SOURCE_AUTHORED));
    }

    #[test]
    fn trusted_project_title_is_returned() {
        // The trust class "trusted" is trusted whatever the space, which
        // includes a project item signed by a trusted key.
        let trust = TrustSummary {
            class: "trusted".into(),
            signer: Some("fp".into()),
            reason: None,
        };
        assert!(is_trusted(Some(&trust)));
        let config = shipped();
        let (title, source) =
            yaml_title(&config, "tool", is_trusted(Some(&trust)), "title: Mine\n");
        assert_eq!((title.as_str(), source), ("Mine", SOURCE_AUTHORED));
    }

    #[test]
    fn only_the_trusted_class_is_trusted() {
        for class in ["untrusted", "unsigned", "unknown"] {
            let trust = TrustSummary {
                class: class.into(),
                signer: None,
                reason: None,
            };
            assert!(!is_trusted(Some(&trust)), "{class}");
        }
        assert!(!is_trusted(None));
    }

    #[test]
    fn untrusted_item_with_a_title_key_gets_the_derived_leaf() {
        let config = shipped();
        let (title, source) = yaml_title(&config, "graph", false, "title: Spoofed\n");
        assert_eq!((title.as_str(), source), ("my fancy item", SOURCE_DERIVED));
    }

    #[test]
    fn heading_source_for_a_markdown_kind() {
        let config = shipped();
        let text = "<!-- ryeos:signed:x:y:z:w -->\n---\ntitle: \"\"\n---\n\n```\n# not this\n```\n# The real heading\n\nbody\n";
        let (title, source) = config.title(
            "knowledge",
            "a/b",
            true,
            Some(&markdown_envelope()),
            || Some(text.to_string()),
            |_| Some(serde_json::json!({"title": ""})),
        );
        assert_eq!(
            (title.as_str(), source),
            ("The real heading", SOURCE_HEADING)
        );
        // A kind whose rule has no heading ignores the same text.
        let (_, source) = config.title(
            "graph",
            "a/b",
            true,
            Some(&markdown_envelope()),
            || Some(text.to_string()),
            |_| None,
        );
        assert_eq!(source, SOURCE_DERIVED);
    }

    #[test]
    fn control_characters_are_stripped_and_length_is_capped() {
        let config = shipped();
        let long = "x".repeat(500);
        let text = format!("title: \"  a\\u0007b\\u001b[31m \\n\\t c  {long}\"\n");
        let (title, source) = yaml_title(&config, "tool", true, &text);
        assert_eq!(source, SOURCE_AUTHORED);
        assert!(title.starts_with("ab[31m c x"), "{title}");
        assert_eq!(title.chars().count(), config.limits.max_chars);
        assert!(!title.chars().any(char::is_control));
    }

    #[test]
    fn invisible_formatting_characters_are_stripped() {
        let config = shipped();
        let text = "title: \"safe\\u202e txt.exe\\u200b\\u2066 name\\ufeff\"\n";
        let (title, source) = yaml_title(&config, "tool", true, text);
        assert_eq!(source, SOURCE_AUTHORED);
        assert_eq!(title, "safe txt.exe name");
    }

    #[test]
    fn unreadable_or_unparseable_item_is_derived() {
        let config = shipped();
        let (_, source) = config.title("tool", "a/b", true, None, || None, |_| None);
        assert_eq!(source, SOURCE_DERIVED);
        let (_, source) = config.title("tool", "a/b", true, None, || Some("x".into()), |_| None);
        assert_eq!(source, SOURCE_DERIVED);
    }

    #[test]
    fn config_with_an_unknown_field_is_refused() {
        let good = "category: c\nversion: '1'\ndescription: d\nkinds: {}\ndefault: {authored: [title]}\nauthored_requires_trust: true\nlimits: {heading_scan_bytes: 10, max_chars: 10}\n";
        let parse = |text: &str| ItemTitleConfig::from_value(serde_yaml::from_str(text).unwrap());
        assert!(parse(good).is_ok());
        assert!(parse(&format!("{good}extra: 1\n")).is_err());
        assert!(
            parse(&good.replace("{authored: [title]}", "{authored: [title], oops: 1}")).is_err()
        );
        assert!(parse(&good.replace("max_chars: 10", "max_chars: 0")).is_err());
    }
}
