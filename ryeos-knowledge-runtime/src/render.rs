//! Header + separator rendering for composed output.

use crate::types::ComposeRole;

/// Render a single item with its role header and separator.
pub fn render_item(role: ComposeRole, item_id: &str, body: &str) -> String {
    let role_label = match role {
        ComposeRole::Primary => "Primary",
        ComposeRole::Extends => "Extends",
        ComposeRole::Reference => "Reference",
    };
    format!("## {}: {}\n\n{}\n\n---\n\n", role_label, item_id, body.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_primary_item() {
        let result = render_item(ComposeRole::Primary, "arc/foundation", "Body text");
        assert!(result.contains("## Primary: arc/foundation"));
        assert!(result.contains("Body text"));
        assert!(result.ends_with("---\n\n"));
    }

    #[test]
    fn render_extends_item() {
        let result = render_item(ComposeRole::Extends, "arc/base", "Base content");
        assert!(result.contains("## Extends: arc/base"));
    }

    #[test]
    fn render_reference_item() {
        let result = render_item(ComposeRole::Reference, "arc/guide", "Guide content");
        assert!(result.contains("## Reference: arc/guide"));
    }
}
