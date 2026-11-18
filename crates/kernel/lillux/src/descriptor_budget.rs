//! Open-descriptor budget for a process that sets up a sandbox from inherited
//! descriptors.
//!
//! A sandbox setup process holds every inherited descriptor it was handed
//! until that source's last mount is bound and verified; only then does it
//! release the source and the private authority derived from it (the
//! re-anchored source, or the byte copy materialized into private staging).
//! It also keeps a few staging directory handles. The limit installed on that process must
//! cover that peak; it is unrelated to the limit the eventual target runs under.

use std::fmt;

/// Descriptors held per inherited descriptor at the setup peak: the original,
/// held until its last mount is bound, and its private authority while that
/// source still has mounts to bind. Transient duplicates (kind inspection,
/// positional copy source, ancestor pin) live one source at a time and are
/// covered by [`SETUP_FIXED_DESCRIPTORS`].
pub const SETUP_DESCRIPTORS_PER_INHERITED: u64 = 2;

/// Fixed setup cost independent of the mount count: stdio (3), request and
/// status descriptors, the report pipe, private root and staging handles
/// (`root`, `mountpoint`, `content`, plus the `current` directory opened by
/// detach and the root clone used while creating files), the proc, namespace
/// and seccomp handles, and slack for the standard library's own opens.
pub const SETUP_FIXED_DESCRIPTORS: u64 = 48;

/// The setup budget does not fit under the policy open-file ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupDescriptorBudgetExceeded {
    pub inherited_descriptors: u64,
    pub needed: u64,
    pub ceiling: u64,
}

impl fmt::Display for SetupDescriptorBudgetExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "sandbox setup with {} inherited descriptors needs {} open files but the policy ceiling is {}",
            self.inherited_descriptors, self.needed, self.ceiling
        )
    }
}

impl std::error::Error for SetupDescriptorBudgetExceeded {}

/// Open-file limit for a sandbox setup process holding
/// `inherited_descriptors` descriptors. With a policy ceiling the result is
/// the need itself (never the ceiling), and a need above the ceiling is
/// refused. Without a ceiling no limit is installed.
pub fn setup_open_files(
    inherited_descriptors: usize,
    policy_ceiling: Option<u64>,
) -> Result<Option<u64>, SetupDescriptorBudgetExceeded> {
    let count = inherited_descriptors as u64;
    let needed = count
        .saturating_mul(SETUP_DESCRIPTORS_PER_INHERITED)
        .saturating_add(SETUP_FIXED_DESCRIPTORS);
    match policy_ceiling {
        Some(ceiling) if needed > ceiling => Err(SetupDescriptorBudgetExceeded {
            inherited_descriptors: count,
            needed,
            ceiling,
        }),
        Some(_) => Ok(Some(needed)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_scales_with_descriptors() {
        let few = setup_open_files(2, Some(4096)).unwrap().unwrap();
        let many = setup_open_files(40, Some(4096)).unwrap().unwrap();
        assert_eq!(
            few,
            SETUP_FIXED_DESCRIPTORS + 2 * SETUP_DESCRIPTORS_PER_INHERITED
        );
        assert_eq!(many - few, 38 * SETUP_DESCRIPTORS_PER_INHERITED);
    }

    #[test]
    fn budget_is_bounded_by_policy_and_refuses_above_it() {
        let exact = SETUP_FIXED_DESCRIPTORS + 10 * SETUP_DESCRIPTORS_PER_INHERITED;
        assert_eq!(setup_open_files(10, Some(exact)).unwrap(), Some(exact));
        let error = setup_open_files(11, Some(exact)).unwrap_err();
        assert_eq!(error.inherited_descriptors, 11);
        assert_eq!(error.ceiling, exact);
        assert!(error.needed > exact);
        let message = error.to_string();
        assert!(message.contains("11") && message.contains(&exact.to_string()));
    }

    #[test]
    fn budget_without_policy_installs_no_limit() {
        assert_eq!(setup_open_files(1000, None).unwrap(), None);
    }
}
