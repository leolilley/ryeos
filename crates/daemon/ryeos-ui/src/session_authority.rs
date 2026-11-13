//! The authority a browser session may hold.
//!
//! A browser session is minted by a principal that may hold far more than any
//! browser needs (the local operator holds `*`). The grant a session retains
//! is the minter's scopes intersected with the node's signed session ceiling,
//! and optionally narrowed again by the minter. Intersection is capability
//! aware: both sides are grant patterns, so the result is the set of patterns
//! that both sides cover, never a string comparison.

use ryeos_state::capability::pattern_covers;

/// Patterns covered by both `left` and `right`.
///
/// For each pair, when one pattern covers the other the narrower one is
/// kept. Pattern pairs whose overlap cannot be proven by coverage (for
/// example two unrelated wildcard shapes) contribute nothing: the
/// intersection fails closed. The result is deduplicated, free of patterns
/// already covered by another member, and sorted.
pub fn intersect_capabilities(left: &[String], right: &[String]) -> Vec<String> {
    let mut common: Vec<String> = Vec::new();
    for a in left {
        for b in right {
            if pattern_covers(a, b) {
                common.push(b.clone());
            } else if pattern_covers(b, a) {
                common.push(a.clone());
            }
        }
    }
    common.sort();
    common.dedup();
    let minimal = common
        .iter()
        .filter(|candidate| {
            !common
                .iter()
                .any(|other| other != *candidate && pattern_covers(other, candidate))
        })
        .cloned()
        .collect();
    minimal
}

#[cfg(test)]
mod tests {
    use super::intersect_capabilities;

    fn caps(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn full_wildcard_minter_yields_the_ceiling_patterns() {
        let ceiling = caps(&["ryeos.execute.service.ui/*", "ryeos.fetch.directive.*"]);
        assert_eq!(
            intersect_capabilities(&caps(&["*"]), &ceiling),
            caps(&["ryeos.execute.service.ui/*", "ryeos.fetch.directive.*"])
        );
    }

    #[test]
    fn literal_minter_cap_inside_a_ceiling_wildcard_survives() {
        assert_eq!(
            intersect_capabilities(
                &caps(&["ryeos.execute.service.ui/launch/mint"]),
                &caps(&["ryeos.execute.service.ui/*"]),
            ),
            caps(&["ryeos.execute.service.ui/launch/mint"])
        );
    }

    #[test]
    fn wider_minter_wildcard_is_cut_to_the_narrower_ceiling_pattern() {
        assert_eq!(
            intersect_capabilities(
                &caps(&["ryeos.execute.service.*"]),
                &caps(&["ryeos.execute.service.ui/*"]),
            ),
            caps(&["ryeos.execute.service.ui/*"])
        );
    }

    #[test]
    fn caps_outside_the_ceiling_are_dropped() {
        assert!(
            intersect_capabilities(
                &caps(&["ryeos.execute.service.identity/authorize-key"]),
                &caps(&["ryeos.execute.service.ui/*"]),
            )
            .is_empty()
        );
        assert!(intersect_capabilities(&caps(&["ryeos.execute.service.ui/*"]), &[]).is_empty());
    }

    #[test]
    fn unprovable_wildcard_overlap_fails_closed() {
        assert!(
            intersect_capabilities(
                &caps(&["ryeos.*.service.ui/*"]),
                &caps(&["ryeos.execute.*"]),
            )
            .is_empty()
        );
    }

    #[test]
    fn result_has_no_duplicates_or_covered_members() {
        assert_eq!(
            intersect_capabilities(
                &caps(&["*"]),
                &caps(&[
                    "ryeos.execute.service.ui/*",
                    "ryeos.execute.service.ui/seat/open",
                    "ryeos.execute.service.ui/*",
                ]),
            ),
            caps(&["ryeos.execute.service.ui/*"])
        );
    }
}
