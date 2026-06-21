//! Corpus + reference integrity validation.
//!
//! Errors are integrity violations that should block use of the corpus
//! (missing roots, dangling edges, unparseable frontmatter). Warnings are
//! quality signals (empty body, missing title/category/tags) that do not
//! invalidate the corpus.

use ryeos_runtime::op_wire::TrustClass;

use crate::frontmatter::{parse_metadata_result, strip_frontmatter};
use crate::types::{KnowledgeError, ValidateOutput, ValidatePayload};

pub fn validate(payload: &ValidatePayload) -> Result<ValidateOutput, KnowledgeError> {
    let items = &payload.items_by_ref;
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    // Roots must exist in the corpus. (Read from `inputs.roots` — the
    // shape the executor sends.)
    for r in &payload.inputs.roots {
        if !items.contains_key(r) {
            errors.push(format!("root not found in corpus: {r}"));
        }
    }

    // Edge endpoints must exist (dangling references are integrity errors).
    for e in &payload.edges {
        if !items.contains_key(&e.from) {
            errors.push(format!("edge from-endpoint missing: {} -> {}", e.from, e.to));
        }
        if !items.contains_key(&e.to) {
            errors.push(format!(
                "dangling edge to-endpoint: {} -> {}",
                e.from, e.to
            ));
        }
    }

    // Per-item: trust + frontmatter/body parse (errors) + quality signals.
    for (item_ref, item) in items {
        // An unsigned item in the corpus is an integrity error — the wire
        // item already carries its trust class, so a corpus that contains
        // unauthenticated members must not validate clean.
        if matches!(item.trust_class, TrustClass::Unsigned) {
            errors.push(format!("unsigned item in corpus: {item_ref}"));
        }

        match strip_frontmatter(&item.raw_content, item_ref) {
            Ok(body) => {
                if body.trim().is_empty() {
                    warnings.push(format!("empty body: {item_ref}"));
                }
            }
            Err(e) => errors.push(format!("frontmatter parse failed for {item_ref}: {e}")),
        }

        // Metadata must parse if present — a malformed YAML frontmatter
        // block (markdown) OR a malformed whole-document YAML item is an
        // integrity error, not a quality warning. Format is keyed off the
        // item's source path so `.yaml`/`.yml` items (advertised by the
        // kind schema) are validated too, not just markdown.
        let source_path = item
            .metadata
            .get("source_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let fm = match parse_metadata_result(&item.raw_content, source_path) {
            Ok(Some(fm)) => fm,
            Ok(None) => serde_json::json!({}),
            Err(reason) => {
                errors.push(format!("malformed metadata for {item_ref}: {reason}"));
                serde_json::json!({})
            }
        };
        if fm
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .is_none()
        {
            warnings.push(format!("missing title: {item_ref}"));
        }
        if fm.get("category").is_none() && fm.get("categories").is_none() {
            warnings.push(format!("missing category: {item_ref}"));
        }
        let has_tags = fm
            .get("tags")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty());
        if !has_tags {
            warnings.push(format!("missing tags: {item_ref}"));
        }

        // `references`, if present, must be a string or array of strings,
        // AND each string must be a syntactically valid ref token. A bad
        // shape or an unparseable token (e.g. "bad ref with spaces")
        // otherwise silently degrades to "no edge" in the corpus projector
        // and the corpus would validate clean despite a broken reference.
        if let Some(refs_val) = fm.get("references") {
            match refs_val {
                serde_json::Value::String(s) => check_ref_token(s, item_ref, &mut errors),
                serde_json::Value::Array(a) if a.iter().all(serde_json::Value::is_string) => {
                    for v in a {
                        check_ref_token(v.as_str().unwrap_or(""), item_ref, &mut errors);
                    }
                }
                _ => errors.push(format!(
                    "invalid `references` for {item_ref}: must be a string or array of strings"
                )),
            }
        }
    }

    Ok(ValidateOutput {
        valid: errors.is_empty(),
        errors,
        warnings,
        item_count: items.len(),
        edge_count: payload.edges.len(),
    })
}

/// Push an error if a declared reference string is not a syntactically
/// valid ref token. This is a runtime-local sanity check (the engine owns
/// full canonicalization/alias expansion) — it catches the junk the corpus
/// projector would otherwise drop to "no edge": empty strings and tokens
/// with whitespace or other illegal characters.
fn check_ref_token(s: &str, item_ref: &str, errors: &mut Vec<String>) {
    if !valid_ref_token(s) {
        errors.push(format!(
            "invalid reference `{s}` in {item_ref}: not a valid ref token"
        ));
    }
}

fn valid_ref_token(s: &str) -> bool {
    let s = s.trim();
    // A ref is `kind:bare/id`, a bare `bare/id`, or an `@alias`. Allowed
    // characters cover those forms; notably whitespace is rejected, which
    // is the silent-drop case the corpus projector cannot canonicalize.
    !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '@')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::op_wire::{EdgeKind, GraphEdge, TrustClass, VerifiedItem};
    use std::collections::BTreeMap;

    fn item(body: &str) -> VerifiedItem {
        VerifiedItem {
            raw_content: body.to_string(),
            raw_content_digest: "d".into(),
            metadata: serde_json::json!({}),
            trust_class: TrustClass::TrustedBundle,
        }
    }

    fn payload(items: &[(&str, &str)], edges: Vec<GraphEdge>, roots: Vec<String>) -> ValidatePayload {
        let mut map = BTreeMap::new();
        for (r, b) in items {
            map.insert(r.to_string(), item(b));
        }
        ValidatePayload {
            items_by_ref: map,
            edges,
            inputs: crate::types::ValidateInputs { roots },
        }
    }

    #[test]
    fn well_formed_corpus_is_valid() {
        let p = payload(
            &[("k/a", "---\ntitle: A\ncategory: x\ntags: [t1]\n---\nbody text")],
            vec![],
            vec!["k/a".into()],
        );
        let out = validate(&p).unwrap();
        assert!(out.valid, "errors: {:?}", out.errors);
        assert!(out.warnings.is_empty(), "warnings: {:?}", out.warnings);
        assert_eq!(out.item_count, 1);
    }

    #[test]
    fn dangling_edge_is_error() {
        let p = payload(
            &[("k/a", "---\ntitle: A\ncategory: x\ntags: [t]\n---\nbody")],
            vec![GraphEdge {
                from: "k/a".into(),
                to: "k/ghost".into(),
                kind: EdgeKind::References,
                depth_from_root: None,
            }],
            vec![],
        );
        let out = validate(&p).unwrap();
        assert!(!out.valid);
        assert!(out.errors.iter().any(|e| e.contains("ghost")));
        assert_eq!(out.edge_count, 1);
    }

    #[test]
    fn missing_root_is_error() {
        let p = payload(&[("k/a", "x")], vec![], vec!["k/missing".into()]);
        let out = validate(&p).unwrap();
        assert!(!out.valid);
        assert!(out.errors.iter().any(|e| e.contains("k/missing")));
    }

    #[test]
    fn quality_signals_are_warnings_not_errors() {
        // No frontmatter title/category/tags, empty body → warnings only.
        let p = payload(&[("k/a", "")], vec![], vec![]);
        let out = validate(&p).unwrap();
        assert!(out.valid, "quality issues must not invalidate the corpus");
        assert!(out.warnings.iter().any(|w| w.contains("empty body")));
        assert!(out.warnings.iter().any(|w| w.contains("missing title")));
        assert!(out.warnings.iter().any(|w| w.contains("missing category")));
        assert!(out.warnings.iter().any(|w| w.contains("missing tags")));
    }

    #[test]
    fn malformed_frontmatter_is_integrity_error() {
        // A present-but-unparseable `---` block must fail validation, not
        // pass with only quality warnings.
        let p = payload(
            &[("k/a", "---\ntitle: A\n  bad: : indent:\n---\nbody")],
            vec![],
            vec![],
        );
        let out = validate(&p).unwrap();
        assert!(!out.valid, "malformed frontmatter must invalidate");
        assert!(out.errors.iter().any(|e| e.contains("malformed metadata")));
    }

    #[test]
    fn html_signed_markdown_body_is_stripped_for_empty_check() {
        // A signed `.md` doc (HTML comment + `---` frontmatter) with a real
        // body must NOT warn "empty body" — the signature/frontmatter is
        // not body. Regression for the strip/parse signature-skip mismatch.
        let doc = "<!-- ryeos:signed:2026-01-01T00:00:00Z:h:s:fp -->\n---\ntitle: A\ncategory: c\ntags: [t]\n---\nReal body content here.";
        let p = payload(&[("k/a", doc)], vec![], vec![]);
        let out = validate(&p).unwrap();
        assert!(out.valid, "errors: {:?}", out.errors);
        assert!(
            !out.warnings.iter().any(|w| w.contains("empty body")),
            "body was wrongly seen as empty: {:?}",
            out.warnings
        );
        // And the frontmatter fields were found (no missing-* warnings).
        assert!(out.warnings.is_empty(), "warnings: {:?}", out.warnings);
    }

    #[test]
    fn unsigned_item_is_integrity_error() {
        let mut map = BTreeMap::new();
        map.insert(
            "k/a".to_string(),
            VerifiedItem {
                raw_content: "---\ntitle: A\ncategory: c\ntags: [t]\n---\nbody".into(),
                raw_content_digest: "d".into(),
                metadata: serde_json::json!({}),
                trust_class: TrustClass::Unsigned,
            },
        );
        let p = ValidatePayload {
            items_by_ref: map,
            edges: vec![],
            inputs: crate::types::ValidateInputs { roots: vec![] },
        };
        let out = validate(&p).unwrap();
        assert!(!out.valid, "unsigned corpus member must invalidate");
        assert!(out.errors.iter().any(|e| e.contains("unsigned item")));
    }

    #[test]
    fn malformed_references_shape_is_error() {
        // `references` present but not a string/array-of-strings.
        let p = payload(
            &[(
                "k/a",
                "---\ntitle: A\ncategory: c\ntags: [t]\nreferences: 123\n---\nbody",
            )],
            vec![],
            vec![],
        );
        let out = validate(&p).unwrap();
        assert!(!out.valid);
        assert!(
            out.errors.iter().any(|e| e.contains("invalid `references`")),
            "errors: {:?}",
            out.errors
        );
    }

    /// Build a `.yaml` knowledge item (whole-document YAML), tagged with a
    /// `.yaml` source_path so format-aware validation parses it as YAML.
    fn yaml_item(body: &str) -> VerifiedItem {
        VerifiedItem {
            raw_content: body.to_string(),
            raw_content_digest: "d".into(),
            metadata: serde_json::json!({ "source_path": "/x/k/a.yaml" }),
            trust_class: TrustClass::TrustedBundle,
        }
    }

    fn yaml_payload(item_ref: &str, body: &str) -> ValidatePayload {
        let mut map = BTreeMap::new();
        map.insert(item_ref.to_string(), yaml_item(body));
        ValidatePayload {
            items_by_ref: map,
            edges: vec![],
            inputs: crate::types::ValidateInputs { roots: vec![] },
        }
    }

    #[test]
    fn malformed_yaml_knowledge_item_invalidates() {
        // A `.yaml` knowledge item with broken YAML must be an integrity
        // error — not treated as opaque body that validates clean.
        let p = yaml_payload("k/a", "title: A\n  bad: : indent:\n");
        let out = validate(&p).unwrap();
        assert!(!out.valid, "malformed YAML item must invalidate");
        assert!(
            out.errors.iter().any(|e| e.contains("malformed metadata")),
            "errors: {:?}",
            out.errors
        );
    }

    #[test]
    fn yaml_references_wrong_type_invalidates() {
        // `references: 123` in a YAML item: now that YAML items are parsed,
        // the shape check fires.
        let p = yaml_payload("k/a", "title: A\ncategory: c\ntags: [t]\nreferences: 123\n");
        let out = validate(&p).unwrap();
        assert!(!out.valid);
        assert!(out.errors.iter().any(|e| e.contains("invalid `references`")));
    }

    #[test]
    fn unparseable_reference_token_invalidates() {
        // Valid shape (array of strings) but a token the corpus projector
        // cannot canonicalize → would silently produce no edge.
        let p = payload(
            &[(
                "k/a",
                "---\ntitle: A\ncategory: c\ntags: [t]\nreferences: [\"bad ref with spaces\"]\n---\nbody",
            )],
            vec![],
            vec![],
        );
        let out = validate(&p).unwrap();
        assert!(!out.valid, "unparseable ref token must invalidate");
        assert!(
            out.errors.iter().any(|e| e.contains("invalid reference")),
            "errors: {:?}",
            out.errors
        );
    }

    #[test]
    fn valid_yaml_item_is_valid() {
        let p = yaml_payload("k/a", "title: A\ncategory: c\ntags: [t]\nreferences: [knowledge:k/b]\n");
        let out = validate(&p).unwrap();
        assert!(out.valid, "errors: {:?}", out.errors);
    }
}
