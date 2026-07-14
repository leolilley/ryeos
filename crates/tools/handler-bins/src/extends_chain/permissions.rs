use regex::Regex;

/// Check whether a granted capability pattern covers a child capability using
/// the same anchored wildcard vocabulary as runtime authorization.
fn capability_covers(granted: &str, child: &str) -> bool {
    if granted == child {
        return true;
    }
    let mut regex_str = String::from("^");
    for ch in granted.chars() {
        match ch {
            '*' => regex_str.push_str(".*"),
            '?' => regex_str.push('.'),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                regex_str.push('\\');
                regex_str.push(ch);
            }
            _ => regex_str.push(ch),
        }
    }
    regex_str.push('$');
    Regex::new(&regex_str)
        .map(|regex| regex.is_match(child))
        .unwrap_or(false)
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
}
