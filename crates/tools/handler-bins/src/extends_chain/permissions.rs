/// One shared coverage check with the engine's post-compose narrowing defense
/// (`ryeos_engine::capability_cover`), so the composer and the engine can
/// never disagree — including pattern-language coverage for wildcard-bearing
/// child declarations.
fn capability_covers(granted: &str, child: &str) -> bool {
    ryeos_engine::capability_cover::capability_pattern_covers(granted, child)
}

/// Retain child capabilities covered by at least one parent capability.
/// Child ordering and duplicates are deliberately preserved.
pub(super) fn narrow_capabilities(child_caps: &[String], parent_caps: &[String]) -> Vec<String> {
    child_caps
        .iter()
        .filter(|child| {
            parent_caps
                .iter()
                .any(|parent| capability_covers(parent, child))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn matching_is_anchored_and_escapes_regex_syntax() {
        assert!(capability_covers(
            "ryeos.execute.tool.*",
            "ryeos.execute.tool.echo"
        ));
        assert!(capability_covers("ryeos.get.vault.?", "ryeos.get.vault.x"));
        assert!(!capability_covers(
            "ryeos.execute.tool.echo",
            "prefix.ryeos.execute.tool.echo"
        ));
        assert!(!capability_covers("cap.+", "cap.anything"));
    }

    #[test]
    fn narrowing_preserves_child_order_and_duplicates() {
        let narrowed = narrow_capabilities(
            &caps(&["cap.b", "cap.a", "cap.b", "denied"]),
            &caps(&["cap.*"]),
        );

        assert_eq!(narrowed, caps(&["cap.b", "cap.a", "cap.b"]));
    }

    #[test]
    fn global_wildcard_covers_every_capability() {
        assert_eq!(
            narrow_capabilities(&caps(&["one", "two/child"]), &caps(&["*"])),
            caps(&["one", "two/child"])
        );
    }

    #[test]
    fn child_wildcards_cannot_outrun_a_narrower_parent_pattern() {
        // A parent `?` narrows to exactly one character; a child `*` would
        // widen it. Language coverage, not text matching, must decide.
        assert!(
            narrow_capabilities(&caps(&["ryeos.get.vault.*"]), &caps(&["ryeos.get.vault.?"]))
                .is_empty()
        );
        // A trailing-star parent provably covers a prefixed child pattern.
        assert_eq!(
            narrow_capabilities(&caps(&["a.b.*"]), &caps(&["a.*"])),
            caps(&["a.b.*"])
        );
    }
}
