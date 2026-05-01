use regex::Regex;
use std::collections::BTreeSet;

pub fn cap_matches(granted: &str, required: &str) -> bool {
    if granted == required {
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
        .map(|re| re.is_match(required))
        .unwrap_or(false)
}

pub fn expand_capabilities(caps: &[String]) -> BTreeSet<String> {
    let mut expanded: BTreeSet<String> = caps.iter().cloned().collect();

    let mut to_add = Vec::new();
    for cap in caps {
        if cap == "rye.*" {
            to_add.push("rye.execute.*".to_string());
            to_add.push("rye.fetch.*".to_string());
            to_add.push("rye.sign.*".to_string());
        } else if let Some(suffix) = cap.strip_prefix("rye.execute.") {
            to_add.push(format!("rye.fetch.{suffix}"));
        } else if let Some(suffix) = cap.strip_prefix("rye.sign.") {
            to_add.push(format!("rye.fetch.{suffix}"));
        }
    }

    for cap in to_add {
        tracing::trace!(raw = %"*", expanded = %cap, "expanded capability");
        expanded.insert(cap);
    }
    expanded
}

pub fn check_capability(granted_caps: &[String], required_cap: &str) -> bool {
    let expanded = expand_capabilities(granted_caps);
    tracing::trace!(required = %required_cap, granted = ?expanded, "checking capability");
    expanded.iter().any(|g| cap_matches(g, required_cap))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        assert!(cap_matches(
            "rye.execute.tool.rye.file-system.fs_read",
            "rye.execute.tool.rye.file-system.fs_read"
        ));
    }

    #[test]
    fn wildcard_suffix_matches() {
        assert!(cap_matches(
            "rye.execute.tool.*",
            "rye.execute.tool.rye.file-system.fs_write"
        ));
    }

    #[test]
    fn wildcard_does_not_cross_boundaries_without_star() {
        assert!(!cap_matches("rye.execute", "rye.execute.tool.foo"));
        assert!(!cap_matches("rye.fetch", "rye.fetch.tool.bar"));
    }

    #[test]
    fn different_namespace_no_match() {
        assert!(!cap_matches(
            "rye.fetch.*",
            "rye.execute.tool.rye.file-system.fs_write"
        ));
    }

    #[test]
    fn question_mark_wildcard() {
        assert!(cap_matches(
            "rye.execute.tool.rye.?.fs_read",
            "rye.execute.tool.rye.x.fs_read"
        ));
        assert!(!cap_matches(
            "rye.execute.tool.rye.?.fs_read",
            "rye.execute.tool.rye.xx.fs_read"
        ));
    }

    #[test]
    fn full_wildcard() {
        assert!(cap_matches("rye.*", "rye.execute.tool.anything"));
        assert!(cap_matches("rye.*", "rye.fetch.directive.anything"));
        assert!(cap_matches("rye.*", "rye.sign.tool.anything"));
    }

    #[test]
    fn execute_implies_fetch() {
        let caps = vec!["rye.execute.*".to_string()];
        assert!(expand_capabilities(&caps).contains("rye.fetch.*"));
    }

    #[test]
    fn sign_implies_fetch() {
        let caps = vec!["rye.sign.tool.foo".to_string()];
        assert!(expand_capabilities(&caps).contains("rye.fetch.tool.foo"));
    }

    #[test]
    fn wildcard_expands_to_all() {
        let caps = vec!["rye.*".to_string()];
        let expanded = expand_capabilities(&caps);
        assert!(expanded.contains("rye.execute.*"));
        assert!(expanded.contains("rye.fetch.*"));
        assert!(expanded.contains("rye.sign.*"));
    }

    #[test]
    fn check_capability_uses_expansion() {
        let granted = vec!["rye.execute.*".to_string()];
        assert!(check_capability(
            &granted,
            "rye.fetch.tool.rye.file-system.fs_read"
        ));
    }

    #[test]
    fn check_capability_exact_match() {
        let granted = vec!["rye.fetch.tool.rye.file-system.fs_read".to_string()];
        assert!(check_capability(
            &granted,
            "rye.fetch.tool.rye.file-system.fs_read"
        ));
    }

    #[test]
    fn no_capabilities_denies_all() {
        let granted: Vec<String> = vec![];
        assert!(!check_capability(&granted, "rye.execute.tool.anything"));
    }
}
