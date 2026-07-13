//! Server-side view embedding for effective surfaces.
//!
//! A surface binds views by `view:` ref; it never defines them. Every
//! daemon path that serves an effective surface (`items.effective`,
//! `ui.session.current`) embeds the composed value of each bound view
//! into the surface's `views` map before the payload leaves the daemon,
//! so renderers receive one complete surface instead of resolving each
//! ref in a follow-up round-trip.
//!
//! A view that fails to resolve embeds `{"degraded": <reason>}` under
//! its ref — the surface still ships, and the pane renders the reason
//! instead of the view. Per-view failures never fail the whole surface.

use std::path::Path;

use serde_json::Value;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::engine::{EffectiveItemRequest, Engine};

/// Collect every `view:`-prefixed ref anywhere in the composed surface
/// value — center `tiles`, edge `slots`, `backdrop`, `library` — skipping
/// the ROOT `views` map only (it holds resolved bindings keyed by ref,
/// not refs to resolve). The skip must not apply to nested `views` keys:
/// each grouped `library` entry is `{ group, views: [ref…] }`, and
/// skipping those lists is exactly how a grouped launcher loses every
/// declared view.
fn collect_view_refs(value: &Value, out: &mut Vec<String>) {
    collect_view_refs_at(value, out, true);
}

fn collect_view_refs_at(value: &Value, out: &mut Vec<String>, at_root: bool) {
    match value {
        Value::String(s) if s.starts_with("view:") => out.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                collect_view_refs_at(item, out, false);
            }
        }
        Value::Object(map) => {
            for (key, v) in map {
                if at_root && key == "views" {
                    continue;
                }
                collect_view_refs_at(v, out, false);
            }
        }
        _ => {}
    }
}

/// Embed every bound view into `composed_value.views`, resolving each
/// unique ref through `resolve`. A failed resolution embeds a
/// `{"degraded": <reason>}` entry under the same key. Returns the
/// failures as `(view_ref, reason)` pairs so callers can additionally
/// report them as diagnostics.
pub fn embed_views_with(
    composed_value: &mut Value,
    mut resolve: impl FnMut(&str) -> Result<Value, String>,
) -> Vec<(String, String)> {
    if !composed_value.is_object() {
        return Vec::new();
    }
    let mut view_refs: Vec<String> = Vec::new();
    collect_view_refs(composed_value, &mut view_refs);
    view_refs.sort();
    view_refs.dedup();
    if view_refs.is_empty() {
        return Vec::new();
    }

    // `views` is never authored inline; anything non-object there is
    // malformed. Reset it so per-ref insertion below cannot panic.
    if composed_value.get("views").is_some_and(|v| !v.is_object()) {
        composed_value["views"] = Value::Object(serde_json::Map::new());
    }

    let mut failures: Vec<(String, String)> = Vec::new();
    for view_ref in view_refs {
        match resolve(&view_ref) {
            Ok(view_value) => {
                composed_value["views"][&view_ref] = view_value;
            }
            Err(reason) => {
                composed_value["views"][&view_ref] =
                    serde_json::json!({ "degraded": reason.clone() });
                failures.push((view_ref, reason));
            }
        }
    }
    failures
}

/// Resolve each bound `view:` ref through the engine's effective-item
/// pipeline and embed the composed view values into the surface.
pub fn embed_surface_views(
    engine: &Engine,
    project_root: Option<&Path>,
    composed_value: &mut Value,
) -> Vec<(String, String)> {
    embed_views_with(composed_value, |view_ref| {
        let item_ref =
            CanonicalRef::parse(view_ref).map_err(|e| format!("invalid view ref: {e}"))?;
        engine
            .effective_item(EffectiveItemRequest {
                item_ref,
                expected_kind: Some("view".to_string()),
                project_root: project_root.map(Path::to_path_buf),
            })
            .map(|effective| effective.composed_value)
            .map_err(|e| e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::embed_views_with;

    fn surface_binding_three_views() -> serde_json::Value {
        json!({
            "name": "test",
            "tiles": ["view:ryeos/chain/timeline"],
            "slots": {
                "bottom": { "content": "view:ryeos/input", "open": true }
            },
            "library": [
                { "group": "Threads", "views": ["view:ryeos/threads/list", "view:ryeos/chain/timeline"] }
            ]
        })
    }

    #[test]
    fn embeds_every_bound_view_once() {
        // Three unique refs across tiles/slots/library (one duplicated) →
        // three embedded bindings, each resolved exactly once.
        let mut surface = surface_binding_three_views();
        let mut resolved: Vec<String> = Vec::new();
        let failures = embed_views_with(&mut surface, |view_ref| {
            resolved.push(view_ref.to_string());
            Ok(json!({ "widget": "rows", "resolved_from": view_ref }))
        });

        assert!(failures.is_empty());
        assert_eq!(resolved.len(), 3, "duplicate refs resolve once");
        let views = surface["views"].as_object().unwrap();
        assert_eq!(views.len(), 3);
        for view_ref in [
            "view:ryeos/chain/timeline",
            "view:ryeos/input",
            "view:ryeos/threads/list",
        ] {
            assert_eq!(views[view_ref]["resolved_from"], view_ref);
        }
    }

    #[test]
    fn failed_ref_embeds_degraded_entry_others_still_embed() {
        // One failing ref records a per-view error and embeds a degraded
        // placeholder; the rest of the surface's views embed normally —
        // a single bad view never fails the whole surface.
        let mut surface = surface_binding_three_views();
        let failures = embed_views_with(&mut surface, |view_ref| {
            if view_ref == "view:ryeos/input" {
                Err("item not found".to_string())
            } else {
                Ok(json!({ "widget": "rows" }))
            }
        });

        assert_eq!(
            failures,
            vec![("view:ryeos/input".to_string(), "item not found".to_string())]
        );
        let views = surface["views"].as_object().unwrap();
        assert_eq!(views.len(), 3, "the failed ref is still keyed");
        assert_eq!(views["view:ryeos/input"]["degraded"], "item not found");
        assert_eq!(views["view:ryeos/threads/list"]["widget"], "rows");
        assert_eq!(views["view:ryeos/chain/timeline"]["widget"], "rows");
    }

    #[test]
    fn already_embedded_views_map_is_not_rewalked() {
        // Refs inside an existing `views` map are resolved bindings keyed
        // by ref, not refs to resolve — the walker must skip them.
        let mut surface = json!({
            "name": "test",
            "tiles": ["view:a/b"],
            "views": { "view:c/d": { "widget": "rows" } }
        });
        let mut resolved: Vec<String> = Vec::new();
        embed_views_with(&mut surface, |view_ref| {
            resolved.push(view_ref.to_string());
            Ok(json!({ "widget": "text" }))
        });
        assert_eq!(resolved, vec!["view:a/b"]);
        // The pre-existing entry survives alongside the new embedding.
        assert_eq!(surface["views"]["view:c/d"]["widget"], "rows");
        assert_eq!(surface["views"]["view:a/b"]["widget"], "text");
    }

    #[test]
    fn malformed_non_object_views_resets_before_embedding() {
        // `views` is never authored inline; a malformed non-object there
        // must not panic the embedder — it resets to a map and embeds.
        let mut surface = json!({ "name": "x", "tiles": ["view:a/b"], "views": "bogus" });
        let failures = embed_views_with(&mut surface, |_| Ok(json!({ "widget": "rows" })));
        assert!(failures.is_empty());
        assert_eq!(surface["views"]["view:a/b"]["widget"], "rows");
    }

    #[test]
    fn surface_without_view_refs_gains_no_views_map() {
        let mut surface = json!({ "name": "empty" });
        let failures = embed_views_with(&mut surface, |_| unreachable!("no refs to resolve"));
        assert!(failures.is_empty());
        assert!(surface.get("views").is_none());
    }

    #[test]
    fn non_object_composed_value_is_untouched() {
        let mut value = json!("view:not/an/object");
        let failures = embed_views_with(&mut value, |_| unreachable!("nothing to embed into"));
        assert!(failures.is_empty());
        assert_eq!(value, json!("view:not/an/object"));
    }
}
