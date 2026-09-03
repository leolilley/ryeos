//! Shared capability-pattern matching for persisted authority contracts.
//!
//! Launch admission and later capsule validation must apply one grant-side
//! wildcard language. Keeping the primitive below the engine lets immutable
//! state objects validate delegated authority without reimplementing the
//! runtime authorizer or depending on a higher layer.

use regex::Regex;

/// Match one granted capability pattern against one required capability.
///
/// `*` matches any run of characters (including `/`) and `?` matches exactly
/// one character. Every other character is literal and the match is anchored.
pub fn grant_matches(granted: &str, required: &str) -> bool {
    if granted == required {
        return true;
    }
    let mut regex = String::from("^");
    for character in granted.chars() {
        match character {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            other => regex.push_str(&regex::escape(&other.to_string())),
        }
    }
    regex.push('$');
    Regex::new(&regex)
        .map(|regex| regex.is_match(required))
        .unwrap_or(false)
}

/// Prove that every capability represented by `child` is granted by
/// `parent`. Literal children use ordinary grant matching. Wildcard-bearing
/// children are accepted only under exact equality or a literal parent prefix
/// followed by one trailing `*`; ambiguous pattern-language inclusion fails
/// closed.
pub fn pattern_covers(parent: &str, child: &str) -> bool {
    if parent == child {
        return true;
    }
    if !child.contains('*') && !child.contains('?') {
        return grant_matches(parent, child);
    }
    if let Some(prefix) = parent.strip_suffix('*')
        && !prefix.contains('*')
        && !prefix.contains('?')
    {
        return child.starts_with(prefix);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{grant_matches, pattern_covers};

    #[test]
    fn grant_side_wildcards_cover_exact_child_capabilities() {
        assert!(grant_matches(
            "ryeos.execute.tool.*",
            "ryeos.execute.tool.echo"
        ));
        assert!(grant_matches("*", "ryeos.execute.tool.echo"));
        assert!(!grant_matches(
            "ryeos.execute.service.*",
            "ryeos.execute.tool.echo"
        ));
    }

    #[test]
    fn pattern_coverage_rejects_narrow_parent_over_broad_child() {
        assert!(pattern_covers(
            "ryeos.execute.tool.*",
            "ryeos.execute.tool.echo"
        ));
        assert!(pattern_covers("ryeos.*", "ryeos.execute.tool.*"));
        assert!(!pattern_covers("ryeos.get.vault.?", "ryeos.get.vault.*"));
    }
}
