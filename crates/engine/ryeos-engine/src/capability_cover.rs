//! One shared, conservative capability-pattern coverage check.
//!
//! Used by the engine's post-compose narrowing defense and the extends-chain
//! composer so the two boundaries can never disagree. Coverage is over the
//! pattern *language*, not pattern text: a child that itself contains
//! wildcards is covered only when every string it can generate is provably
//! matched by the parent. Unprovable combinations fail closed.

/// Does `parent` (anchored glob: `*` any run, `?` any one char) cover `child`?
///
/// `child` may itself be a pattern. Rules, in order:
/// 1. exact equality covers;
/// 2. a literal child (no wildcards) is covered by ordinary glob match;
/// 3. a wildcard-bearing child is covered only when `parent` is a literal
///    prefix followed by a single trailing `*`, and the child starts with
///    that literal prefix — every string the child generates then shares the
///    prefix and is matched by the parent's trailing `*`;
/// 4. everything else is not covered (conservative: a parent `?` never covers
///    a child `*`, and interior parent wildcards never cover child patterns).
pub fn capability_pattern_covers(parent: &str, child: &str) -> bool {
    if parent == child {
        return true;
    }
    let child_is_pattern = child.contains('*') || child.contains('?');
    if !child_is_pattern {
        return glob_match(parent, child);
    }
    if let Some(prefix) = parent.strip_suffix('*')
        && !prefix.contains('*')
        && !prefix.contains('?')
    {
        return child.starts_with(prefix);
    }
    false
}

fn glob_match(pattern: &str, value: &str) -> bool {
    let mut regex = String::from("^");
    for character in pattern.chars() {
        match character {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            other => regex.push_str(&regex::escape(&other.to_string())),
        }
    }
    regex.push('$');
    regex::Regex::new(&regex)
        .map(|regex| regex.is_match(value))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::capability_pattern_covers;

    #[test]
    fn literal_children_use_glob_semantics() {
        assert!(capability_pattern_covers("a.b.*", "a.b.c"));
        assert!(capability_pattern_covers("a.?.c", "a.b.c"));
        assert!(!capability_pattern_covers("a.b.*", "a.x.c"));
    }

    #[test]
    fn wildcard_children_require_provable_language_inclusion() {
        assert!(capability_pattern_covers("a.b.*", "a.b.*"));
        assert!(capability_pattern_covers("a.*", "a.b.*"));
        assert!(capability_pattern_covers("ryeos.execute.tool.arc/*", "ryeos.execute.tool.arc/x*"));
        // The blind spot this module exists to close: `?` must never cover `*`.
        assert!(!capability_pattern_covers("ryeos.get.vault.?", "ryeos.get.vault.*"));
        // Interior parent wildcards cannot prove coverage of a child pattern.
        assert!(!capability_pattern_covers("a.*.c", "a.b.*"));
        // A child `?` under a non-star parent fails closed.
        assert!(!capability_pattern_covers("a.b.c", "a.b.?"));
    }
}
