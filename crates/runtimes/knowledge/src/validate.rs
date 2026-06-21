//! Corpus + reference integrity validation.
//!
//! Errors are integrity violations that should block use of the corpus
//! (missing roots, dangling edges, unparseable frontmatter). Warnings are
//! quality signals (empty body, missing title/category/tags) that do not
//! invalidate the corpus.

use ryeos_runtime::op_wire::TrustClass;

use crate::frontmatter::{parse_frontmatter_result, strip_frontmatter};
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

        // Frontmatter must parse if a block is present — a malformed YAML
        // frontmatter block is an integrity error, not a quality warning.
        let fm = match parse_frontmatter_result(&item.raw_content) {
            Ok(Some(fm)) => fm,
            Ok(None) => serde_json::json!({}),
            Err(reason) => {
                errors.push(format!("malformed frontmatter for {item_ref}: {reason}"));
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

        // `references`, if present, must be a string or array of strings.
        // A malformed shape (null, number, object, mixed array) otherwise
        // silently degrades to "no edges" in the executor projector.
        if let Some(refs_val) = fm.get("references") {
            let ok = match refs_val {
                serde_json::Value::String(_) => true,
                serde_json::Value::Array(a) => a.iter().all(serde_json::Value::is_string),
                _ => false,
            };
            if !ok {
                errors.push(format!(
                    "invalid `references` for {item_ref}: must be a string or array of strings"
                ));
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
        assert!(out.errors.iter().any(|e| e.contains("malformed frontmatter")));
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
}
