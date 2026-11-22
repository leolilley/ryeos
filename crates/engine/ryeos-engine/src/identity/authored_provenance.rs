//! Detection of the provenance line the daemon appends to a runtime-authored
//! item before signing it with the node key.
//!
//! The daemon writes `<comment-prefix> ryeos:authored: {json}[ <suffix>]` at
//! the end of the body and then signs the whole body, so the line sits inside
//! the signed bytes. Runtimes may not submit content containing the marker.
//! The marker on its own proves nothing about who signed: an item is
//! runtime-authored only when the node key signed it AND the line is present.
//! Callers that know the node fingerprint make that join.

/// Marker that opens the provenance line of a runtime-authored item.
pub const PROVENANCE_MARKER: &str = "ryeos:authored:";

/// Whether `raw_content` (signature line already stripped) carries a
/// well-formed runtime provenance line: a comment-only prefix, the marker,
/// then a JSON object whose `authored_by` is `"runtime"`. The check is
/// format-agnostic, so it holds for every kind's comment envelope.
pub fn has_runtime_provenance_line(raw_content: &str) -> bool {
    raw_content.lines().any(is_runtime_provenance_line)
}

fn is_runtime_provenance_line(line: &str) -> bool {
    let Some((lead, rest)) = line.trim().split_once(PROVENANCE_MARKER) else {
        return false;
    };
    let lead = lead.trim();
    if lead.is_empty()
        || lead
            .chars()
            .any(|c| c.is_alphanumeric() || c.is_whitespace())
    {
        return false;
    }
    let rest = rest.trim_start();
    let Some(end) = rest.rfind('}') else {
        return false;
    };
    if !rest.starts_with('{') {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&rest[..=end]).is_ok_and(|json| {
        json.get("authored_by").and_then(serde_json::Value::as_str) == Some("runtime")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_and_yaml_provenance_lines_are_detected() {
        let md = "# Title\n\nBody.\n\n<!-- ryeos:authored: {\"authored_by\":\"runtime\",\"content_digest\":\"ab\"} -->\n";
        assert!(has_runtime_provenance_line(md));
        let yaml = "name: x\n\n# ryeos:authored: {\"authored_by\":\"runtime\"}\n";
        assert!(has_runtime_provenance_line(yaml));
    }

    #[test]
    fn prose_mentioning_the_marker_is_not_provenance() {
        assert!(!has_runtime_provenance_line(
            "Runtime items carry a ryeos:authored: {\"authored_by\":\"runtime\"} line.\n"
        ));
        assert!(!has_runtime_provenance_line(
            "Look for `ryeos:authored:` in the footer.\n"
        ));
    }

    #[test]
    fn malformed_or_foreign_provenance_is_not_runtime_authored() {
        assert!(!has_runtime_provenance_line("# ryeos:authored: not json\n"));
        assert!(!has_runtime_provenance_line(
            "# ryeos:authored: {\"authored_by\":\"operator\"}\n"
        ));
        assert!(!has_runtime_provenance_line("plain body\n"));
    }
}
