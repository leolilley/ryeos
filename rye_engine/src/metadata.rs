//! Lightweight metadata extraction for executor discovery.
//!
//! Parsers are registered in a `MetadataParserRegistry` by parser ID.
//! Each parser returns raw `serde_json::Value` — a generic parsed
//! document with no normalization. Extraction rules from the kind
//! schema then map raw fields into `ItemMetadata`.

use std::collections::HashMap;
use std::path::Path;

use regex::Regex;
use serde_json::{Map, Value};

use crate::contracts::ItemMetadata;
use crate::error::EngineError;
use crate::kind_registry::ExtractionRule;

/// A metadata parser function: takes file content, returns raw parsed data.
pub type MetadataParserFn = Box<dyn Fn(&str) -> Result<Value, EngineError> + Send + Sync>;

/// Registry mapping parser IDs to parser functions.
///
/// Parser IDs come from kind schemas (e.g. `"python/ast"`,
/// `"yaml/yaml"`). The engine never hardcodes which parsers exist —
/// they're registered at construction time.
pub struct MetadataParserRegistry {
    parsers: HashMap<String, MetadataParserFn>,
}

impl MetadataParserRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            parsers: HashMap::new(),
        }
    }

    /// Register the built-in parsers shipped with the engine.
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        reg.register("python/ast", Box::new(extract_python));
        reg.register("yaml/yaml", Box::new(extract_yaml));
        reg.register("markdown/xml", Box::new(extract_markdown_xml));
        reg.register("markdown/frontmatter", Box::new(extract_markdown_frontmatter));
        reg
    }

    /// Register a parser function for a parser ID.
    pub fn register(&mut self, parser_id: &str, parser: MetadataParserFn) {
        self.parsers.insert(parser_id.to_owned(), parser);
    }

    /// Extract raw parsed data using the parser registered for `parser_id`.
    ///
    /// Unregistered parser IDs return `EngineError::ParserNotRegistered`.
    pub fn extract(&self, content: &str, parser_id: &str) -> Result<Value, EngineError> {
        tracing::trace!(parser_id = parser_id, "extracting metadata");
        match self.parsers.get(parser_id) {
            Some(parser) => parser(content),
            None => Err(EngineError::ParserNotRegistered {
                parser_id: parser_id.to_owned(),
            }),
        }
    }

    /// Check whether a parser ID is registered.
    pub fn contains(&self, parser_id: &str) -> bool {
        self.parsers.contains_key(parser_id)
    }
}

impl Default for MetadataParserRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl std::fmt::Debug for MetadataParserRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetadataParserRegistry")
            .field("parser_ids", &self.parsers.keys().collect::<Vec<_>>())
            .finish()
    }
}

// ── Extraction rule application ──────────────────────────────────────

/// Apply extraction rules to a raw parsed document, producing `ItemMetadata`.
///
/// This is the ONLY place that maps rule output names to `ItemMetadata` fields.
pub fn apply_extraction_rules(
    parsed: &Value,
    rules: &HashMap<String, ExtractionRule>,
    file_path: &Path,
) -> ItemMetadata {
    let mut metadata = ItemMetadata::default();

    for (field, rule) in rules {
        let value = match rule {
            ExtractionRule::Filename => file_path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_owned()),
            ExtractionRule::Constant { value } => Some(value.clone()),
            ExtractionRule::Path { key } => extract_string_from_value(parsed, key),
        };

        if let Some(val) = value {
            match field.as_str() {
                "executor_id" => metadata.executor_id = Some(val),
                "version" => metadata.version = Some(val),
                "description" => metadata.description = Some(val),
                "category" => metadata.category = Some(val),
                _ => {
                    metadata.extra.insert(field.clone(), Value::String(val));
                }
            }
        }
    }

    metadata
}

/// Extract a string value from a `Value` by key lookup.
fn extract_string_from_value(parsed: &Value, key: &str) -> Option<String> {
    let val = parsed.get(key)?;
    match val {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

// ── Built-in parser implementations ──────────────────────────────────
//
// These are plain functions, not engine internals. They're registered
// into the registry at construction time via `with_builtins()`.
// Each returns raw `Value` — no normalization into `ItemMetadata`.

/// Extract dunder variables (`__version__`, `__executor_id__`, etc.)
/// from Python source using regex. Returns raw key-value pairs.
pub fn extract_python(content: &str) -> Result<Value, EngineError> {
    let re = Regex::new(r#"(?m)^__(\w+)__\s*=\s*["']([^"']+)["']"#).unwrap();

    let mut map = Map::new();
    for cap in re.captures_iter(content) {
        let key = format!("__{}__", &cap[1]);
        let val = cap[2].to_string();
        map.insert(key, Value::String(val));
    }
    Ok(Value::Object(map))
}

/// Parse YAML content and return as `Value`.
pub fn extract_yaml(content: &str) -> Result<Value, EngineError> {
    let cleaned = strip_signature_lines(content);
    let val: Value = serde_yaml::from_str(&cleaned)
        .map_err(|e| EngineError::Internal(format!("YAML parse error: {e}")))?;
    Ok(val)
}

/// Extract metadata from markdown with an XML code fence (directives).
/// Returns extracted fields as a `Value::Object`.
pub fn extract_markdown_xml(content: &str) -> Result<Value, EngineError> {
    let xml = extract_fenced_block(content, "xml").unwrap_or_default();
    if xml.is_empty() {
        return Ok(Value::Object(Map::new()));
    }

    let mut map = Map::new();

    if let Some(cap) = Regex::new(r#"version="([^"]+)""#)
        .unwrap()
        .captures(&xml)
    {
        map.insert("version".to_string(), Value::String(cap[1].to_string()));
    }

    if let Some(cap) = Regex::new(r"<description>([\s\S]*?)</description>")
        .unwrap()
        .captures(&xml)
    {
        map.insert("description".to_string(), Value::String(cap[1].trim().to_string()));
    }

    if let Some(cap) = Regex::new(r"<category>([\s\S]*?)</category>")
        .unwrap()
        .captures(&xml)
    {
        map.insert("category".to_string(), Value::String(cap[1].trim().to_string()));
    }

    if let Some(cap) = Regex::new(r#"<directive\s[^>]*name="([^"]+)""#)
        .unwrap()
        .captures(&xml)
    {
        map.insert("name".to_string(), Value::String(cap[1].to_string()));
    }

    Ok(Value::Object(map))
}

/// Extract metadata from markdown with a YAML code fence (knowledge).
/// Returns the parsed YAML as `Value`.
pub fn extract_markdown_frontmatter(content: &str) -> Result<Value, EngineError> {
    let yaml = extract_fenced_block(content, "yaml").unwrap_or_default();
    if yaml.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let val: Value = serde_yaml::from_str(&yaml)
        .map_err(|e| EngineError::Internal(format!("YAML parse error: {e}")))?;
    Ok(val)
}

// ── Shared helpers ──────────────────────────────────────────────────

fn strip_signature_lines(content: &str) -> String {
    content
        .lines()
        .filter(|line| !line.starts_with("# rye:signed:"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_fenced_block(content: &str, lang: &str) -> Option<String> {
    let open = format!("```{lang}");
    let mut in_block = false;
    let mut lines = Vec::new();

    for line in content.lines() {
        if !in_block {
            if line.trim().starts_with(&open) {
                in_block = true;
            }
        } else if line.trim() == "```" {
            return Some(lines.join("\n"));
        } else {
            lines.push(line);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> MetadataParserRegistry {
        MetadataParserRegistry::with_builtins()
    }

    fn rules_for_python() -> HashMap<String, ExtractionRule> {
        let mut rules = HashMap::new();
        rules.insert("version".into(), ExtractionRule::Path { key: "__version__".into() });
        rules.insert("executor_id".into(), ExtractionRule::Path { key: "__executor_id__".into() });
        rules.insert("description".into(), ExtractionRule::Path { key: "__tool_description__".into() });
        rules.insert("category".into(), ExtractionRule::Path { key: "__category__".into() });
        rules
    }

    fn rules_for_yaml() -> HashMap<String, ExtractionRule> {
        let mut rules = HashMap::new();
        rules.insert("version".into(), ExtractionRule::Path { key: "version".into() });
        rules.insert("executor_id".into(), ExtractionRule::Path { key: "executor_id".into() });
        rules.insert("description".into(), ExtractionRule::Path { key: "description".into() });
        rules.insert("category".into(), ExtractionRule::Path { key: "category".into() });
        rules
    }

    fn rules_for_xml() -> HashMap<String, ExtractionRule> {
        let mut rules = HashMap::new();
        rules.insert("version".into(), ExtractionRule::Path { key: "version".into() });
        rules.insert("description".into(), ExtractionRule::Path { key: "description".into() });
        rules.insert("category".into(), ExtractionRule::Path { key: "category".into() });
        rules
    }

    fn fake_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("/fake/{name}.py"))
    }

    #[test]
    fn python_ast_extracts_dunders() {
        let src = "__version__ = \"1.0.0\"\n__executor_id__ = \"test\"\n__tool_description__ = \"A test tool\"\n__category__ = \"test/category\"\n__author__ = \"someone\"\n";
        let parsed = registry().extract(src, "python/ast").unwrap();

        let mut rules = rules_for_python();
        rules.insert("author".into(), ExtractionRule::Path { key: "__author__".into() });
        let m = apply_extraction_rules(&parsed, &rules, &fake_path("test"));

        assert_eq!(m.version.as_deref(), Some("1.0.0"));
        assert_eq!(m.executor_id.as_deref(), Some("test"));
        assert_eq!(m.description.as_deref(), Some("A test tool"));
        assert_eq!(m.category.as_deref(), Some("test/category"));
        assert_eq!(m.extra.get("author"), Some(&Value::String("someone".into())));
    }

    #[test]
    fn python_ast_handles_no_metadata() {
        let src = "print('hello')\n";
        let parsed = registry().extract(src, "python/ast").unwrap();
        let m = apply_extraction_rules(&parsed, &rules_for_python(), &fake_path("test"));
        assert_eq!(m.version, None);
        assert_eq!(m.executor_id, None);
    }

    #[test]
    fn yaml_extracts_metadata() {
        let src = "version: \"1.0.0\"\ndescription: \"A graph\"\ncategory: \"test\"\nexecutor_id: \"native:graph_walker\"\n";
        let parsed = registry().extract(src, "yaml/yaml").unwrap();
        let m = apply_extraction_rules(&parsed, &rules_for_yaml(), &fake_path("test"));
        assert_eq!(m.version.as_deref(), Some("1.0.0"));
        assert_eq!(m.executor_id.as_deref(), Some("native:graph_walker"));
    }

    #[test]
    fn yaml_strips_signature() {
        let src = "# rye:signed:abc123\nversion: \"1.0.0\"\n";
        let parsed = registry().extract(src, "yaml/yaml").unwrap();
        let m = apply_extraction_rules(&parsed, &rules_for_yaml(), &fake_path("test"));
        assert_eq!(m.version.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn markdown_xml_extracts_directive() {
        let src = "# My Directive\n\n```xml\n<directive name=\"my_directive\" version=\"2.0.0\">\n  <description>Does useful things</description>\n  <category>workflow</category>\n</directive>\n```\n\nBody content here.\n";
        let parsed = registry().extract(src, "markdown/xml").unwrap();
        let m = apply_extraction_rules(&parsed, &rules_for_xml(), &fake_path("test"));
        assert_eq!(m.version.as_deref(), Some("2.0.0"));
        assert_eq!(m.description.as_deref(), Some("Does useful things"));
        assert_eq!(m.category.as_deref(), Some("workflow"));
        assert_eq!(m.executor_id, None);
    }

    #[test]
    fn markdown_frontmatter_extracts_knowledge() {
        let src = "# Knowledge Item\n\n```yaml\nversion: \"1.0.0\"\ndescription: \"Important knowledge\"\ncategory: \"reference\"\n```\n\nContent here.\n";
        let parsed = registry().extract(src, "markdown/frontmatter").unwrap();
        let m = apply_extraction_rules(&parsed, &rules_for_yaml(), &fake_path("test"));
        assert_eq!(m.version.as_deref(), Some("1.0.0"));
        assert_eq!(m.description.as_deref(), Some("Important knowledge"));
    }

    #[test]
    fn parser_not_registered_fails() {
        let err = registry().extract("anything", "unknown/parser").unwrap_err();
        assert!(
            matches!(err, EngineError::ParserNotRegistered { ref parser_id } if parser_id == "unknown/parser"),
            "expected ParserNotRegistered, got: {err:?}"
        );
    }

    #[test]
    fn custom_parser_can_be_registered() {
        let mut reg = MetadataParserRegistry::new();
        reg.register(
            "custom/toml",
            Box::new(|content: &str| {
                let mut map = Map::new();
                if content.contains("version") {
                    map.insert("version".into(), Value::String("custom".into()));
                }
                Ok(Value::Object(map))
            }),
        );

        let parsed = reg.extract("version = true", "custom/toml").unwrap();
        let rules = {
            let mut r = HashMap::new();
            r.insert("version".into(), ExtractionRule::Path { key: "version".into() });
            r
        };
        let m = apply_extraction_rules(&parsed, &rules, &fake_path("test"));
        assert_eq!(m.version.as_deref(), Some("custom"));

        // Builtins not available since we used new() not with_builtins()
        let err = reg.extract("__version__ = \"1.0\"", "python/ast").unwrap_err();
        assert!(matches!(err, EngineError::ParserNotRegistered { .. }));
    }

    #[test]
    fn extraction_rule_filename() {
        let parsed = Value::Object(Map::new());
        let mut rules = HashMap::new();
        rules.insert("name".into(), ExtractionRule::Filename);
        let m = apply_extraction_rules(&parsed, &rules, &Path::new("/project/.ai/tools/my_tool.py"));
        assert_eq!(m.extra.get("name"), Some(&Value::String("my_tool".into())));
    }

    #[test]
    fn extraction_rule_constant() {
        let parsed = Value::Object(Map::new());
        let mut rules = HashMap::new();
        rules.insert("executor_id".into(), ExtractionRule::Constant { value: "@primitive_chain".into() });
        let m = apply_extraction_rules(&parsed, &rules, &fake_path("test"));
        assert_eq!(m.executor_id.as_deref(), Some("@primitive_chain"));
    }

    #[test]
    fn extraction_rule_path() {
        let mut map = Map::new();
        map.insert("__version__".into(), Value::String("2.5.0".into()));
        let parsed = Value::Object(map);
        let mut rules = HashMap::new();
        rules.insert("version".into(), ExtractionRule::Path { key: "__version__".into() });
        let m = apply_extraction_rules(&parsed, &rules, &fake_path("test"));
        assert_eq!(m.version.as_deref(), Some("2.5.0"));
    }
}
