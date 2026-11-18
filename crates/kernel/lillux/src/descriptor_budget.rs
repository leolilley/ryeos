//! Open-descriptor budget for a process that sets up a sandbox from inherited
//! descriptors.
//!
//! A sandbox setup process holds every inherited descriptor it was handed
//! until that source's last mount is bound and verified; only then does it
//! release the source and the private authority derived from it (the
//! re-anchored source, or the byte copy materialized into private staging).
//! It also keeps a few staging directory handles. The limit installed on that process must
//! cover that peak; it is unrelated to the limit the eventual target runs under.
//!
//! The setup process also places target slots: fixed child descriptor numbers
//! (a sealed payload's slot, a channel above stdio) that it duplicates onto
//! before the target's own limit applies. Placing onto a number at or above
//! the open-file limit fails, so the limit must also exceed the highest slot,
//! whatever the descriptor count.

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
    pub highest_target_slot: Option<u32>,
    pub needed: u64,
    pub ceiling: u64,
}

impl fmt::Display for SetupDescriptorBudgetExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "sandbox setup with {} inherited descriptors",
            self.inherited_descriptors
        )?;
        if let Some(slot) = self.highest_target_slot {
            write!(formatter, " and target slots up to {slot}")?;
        }
        write!(
            formatter,
            " needs {} open files but the policy ceiling is {}",
            self.needed, self.ceiling
        )
    }
}

impl std::error::Error for SetupDescriptorBudgetExceeded {}

/// Open-file limit for a sandbox setup process holding
/// `inherited_descriptors` descriptors and placing target slots up to
/// `highest_target_slot`. The need is the larger of the descriptor peak and
/// one past the highest slot. With a policy ceiling the result is the need
/// itself (never the ceiling), and a need above the ceiling is refused.
/// Without a ceiling no limit is installed.
pub fn setup_open_files(
    inherited_descriptors: usize,
    highest_target_slot: Option<u32>,
    policy_ceiling: Option<u64>,
) -> Result<Option<u64>, SetupDescriptorBudgetExceeded> {
    let count = inherited_descriptors as u64;
    let peak = count
        .saturating_mul(SETUP_DESCRIPTORS_PER_INHERITED)
        .saturating_add(SETUP_FIXED_DESCRIPTORS);
    let slots = highest_target_slot.map_or(0, |slot| u64::from(slot) + 1);
    let needed = peak.max(slots);
    match policy_ceiling {
        Some(ceiling) if needed > ceiling => Err(SetupDescriptorBudgetExceeded {
            inherited_descriptors: count,
            highest_target_slot,
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
        let few = setup_open_files(2, None, Some(4096)).unwrap().unwrap();
        let many = setup_open_files(40, None, Some(4096)).unwrap().unwrap();
        assert_eq!(
            few,
            SETUP_FIXED_DESCRIPTORS + 2 * SETUP_DESCRIPTORS_PER_INHERITED
        );
        assert_eq!(many - few, 38 * SETUP_DESCRIPTORS_PER_INHERITED);
    }

    #[test]
    fn budget_is_bounded_by_policy_and_refuses_above_it() {
        let exact = SETUP_FIXED_DESCRIPTORS + 10 * SETUP_DESCRIPTORS_PER_INHERITED;
        assert_eq!(
            setup_open_files(10, None, Some(exact)).unwrap(),
            Some(exact)
        );
        let error = setup_open_files(11, None, Some(exact)).unwrap_err();
        assert_eq!(error.inherited_descriptors, 11);
        assert_eq!(error.ceiling, exact);
        assert!(error.needed > exact);
        let message = error.to_string();
        assert!(message.contains("11") && message.contains(&exact.to_string()));
    }

    #[test]
    fn budget_covers_the_highest_target_slot() {
        // Few descriptors, but a sealed payload placed at a high fixed slot.
        let limit = setup_open_files(3, Some(198), Some(4096)).unwrap().unwrap();
        assert_eq!(limit, 199);
        // A large descriptor peak still wins over a low slot.
        let peak = setup_open_files(200, Some(4), Some(4096)).unwrap().unwrap();
        assert_eq!(
            peak,
            SETUP_FIXED_DESCRIPTORS + 200 * SETUP_DESCRIPTORS_PER_INHERITED
        );
        let error = setup_open_files(3, Some(198), Some(128)).unwrap_err();
        assert_eq!(error.highest_target_slot, Some(198));
        assert_eq!(error.needed, 199);
        assert!(error.to_string().contains("target slots up to 198"));
    }

    #[test]
    fn budget_without_policy_installs_no_limit() {
        assert_eq!(setup_open_files(1000, None, None).unwrap(), None);
    }
}
