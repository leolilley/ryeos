//! Header + separator rendering for composed output.

use ryeos_runtime::method_wire::TrustClass;

use crate::types::ComposeRole;

/// Header note for an item an operator admitted through launch-trust
/// policy rather than one a pinned key signed.
pub const UNTRUSTED_PROJECT_NOTE: &str =
    "untrusted_project: signed by an unpinned key, admitted by node policy";

/// Header note for an unsigned item. The launch gate refuses unsigned
/// items, so this only renders if that gate is bypassed; it is labelled
/// rather than passed off as pinned content.
pub const UNSIGNED_NOTE: &str = "unsigned: no signature";

/// The trust note an item's header carries, or `None` for content signed
/// by a pinned key. Pinned items render exactly as they did before trust
/// notes existed, so their compose output and token estimates are stable.
pub fn trust_note(trust_class: TrustClass) -> Option<&'static str> {
    match trust_class {
        TrustClass::TrustedBundle | TrustClass::TrustedProject | TrustClass::TrustedNode => None,
        TrustClass::UntrustedProject => Some(UNTRUSTED_PROJECT_NOTE),
        TrustClass::Unsigned => Some(UNSIGNED_NOTE),
    }
}

/// Render a single item with its role header and separator. An item not
/// signed by a pinned key carries its trust note in the header.
pub fn render_item(
    role: ComposeRole,
    item_id: &str,
    trust_class: TrustClass,
    body: &str,
) -> String {
    let role_label = match role {
        ComposeRole::Primary => "Primary",
        ComposeRole::Extends => "Extends",
        ComposeRole::Reference => "Reference",
    };
    let note = trust_note(trust_class)
        .map(|note| format!(" ({note})"))
        .unwrap_or_default();
    format!(
        "## {}: {}{}\n\n{}\n\n---\n\n",
        role_label,
        item_id,
        note,
        body.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_primary_item() {
        let result = render_item(
            ComposeRole::Primary,
            "arc/foundation",
            TrustClass::TrustedBundle,
            "Body text",
        );
        assert!(result.contains("## Primary: arc/foundation"));
        assert!(result.contains("Body text"));
        assert!(result.ends_with("---\n\n"));
    }

    #[test]
    fn render_extends_item() {
        let result = render_item(
            ComposeRole::Extends,
            "arc/base",
            TrustClass::TrustedBundle,
            "Base content",
        );
        assert!(result.contains("## Extends: arc/base"));
    }

    #[test]
    fn render_reference_item() {
        let result = render_item(
            ComposeRole::Reference,
            "arc/guide",
            TrustClass::TrustedBundle,
            "Guide content",
        );
        assert!(result.contains("## Reference: arc/guide"));
    }

    #[test]
    fn render_pinned_item_is_unchanged() {
        for trust_class in [
            TrustClass::TrustedBundle,
            TrustClass::TrustedProject,
            TrustClass::TrustedNode,
        ] {
            assert_eq!(
                render_item(ComposeRole::Reference, "notes/x", trust_class, "  Body  "),
                "## Reference: notes/x\n\nBody\n\n---\n\n",
                "{trust_class:?} must render byte-identical to an unlabelled item"
            );
        }
    }

    #[test]
    fn render_untrusted_reference_is_labelled() {
        assert_eq!(
            render_item(
                ComposeRole::Reference,
                "notes/x",
                TrustClass::UntrustedProject,
                "Body"
            ),
            "## Reference: notes/x (untrusted_project: signed by an unpinned key, \
             admitted by node policy)\n\nBody\n\n---\n\n"
        );
    }

    #[test]
    fn render_unsigned_item_is_labelled() {
        let result = render_item(
            ComposeRole::Primary,
            "notes/x",
            TrustClass::Unsigned,
            "Body",
        );
        assert!(result.starts_with("## Primary: notes/x (unsigned: no signature)\n"));
    }
}
