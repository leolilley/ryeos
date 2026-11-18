//! Open-descriptor budget for a process that sets up a sandbox from inherited
//! descriptors.
//!
//! A sandbox setup process keeps every inherited descriptor it was handed open
//! for the whole setup. Before the first mount it derives a private authority
//! for every non-sealed source (the re-anchored source); a sealed source's
//! private authority (its byte copy in private staging) is made at its first
//! mount. A private authority is released once its source's last mount is
//! bound and verified; the inherited original is not. The peak is therefore
//! two descriptors per inherited non-sealed source, reached before any mount,
//! plus a few staging directory handles. The limit installed on that process
//! must cover that peak.
//!
//! The target inherits that limit. Lillux installs soft and hard limits
//! together, so nothing below the setup process can raise it: a target with no
//! explicit open-file limit runs under the setup limit, which is therefore the
//! policy ceiling, and a target with an explicit limit needs the setup limit
//! at least that high.
//!
//! The setup process also places target slots: fixed child descriptor numbers
//! (a sealed payload's slot, a channel above stdio) that it duplicates onto
//! before the target's own limit applies. Placing onto a number at or above
//! the open-file limit fails, so the limit must also exceed the highest slot,
//! whatever the descriptor count.

use std::fmt;

/// Descriptors held per inherited descriptor at the setup peak: the original,
/// open for the whole setup, and its private authority while that source still
/// has mounts to bind (all non-sealed ones exist at once before the first
/// mount). Transient duplicates (kind inspection,
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
/// `highest_target_slot`, whose target later installs `target_open_files`
/// (`None`: the target keeps the inherited limit).
///
/// The setup need is the larger of the descriptor peak and one past the
/// highest slot; a need above the policy ceiling is refused. Under a ceiling,
/// a target with an explicit limit gets a setup limit of the larger of the
/// need and that limit, and a target without one gets the ceiling itself,
/// since it inherits the setup limit and cannot raise it. Without a ceiling
/// no limit is installed.
pub fn setup_open_files(
    inherited_descriptors: usize,
    highest_target_slot: Option<u32>,
    target_open_files: Option<u64>,
    policy_ceiling: Option<u64>,
) -> Result<Option<u64>, SetupDescriptorBudgetExceeded> {
    let count = inherited_descriptors as u64;
    let peak = count
        .saturating_mul(SETUP_DESCRIPTORS_PER_INHERITED)
        .saturating_add(SETUP_FIXED_DESCRIPTORS);
    let slots = highest_target_slot.map_or(0, |slot| u64::from(slot) + 1);
    let needed = peak.max(slots);
    let Some(ceiling) = policy_ceiling else {
        return Ok(None);
    };
    if needed > ceiling {
        return Err(SetupDescriptorBudgetExceeded {
            inherited_descriptors: count,
            highest_target_slot,
            needed,
            ceiling,
        });
    }
    Ok(Some(match target_open_files {
        Some(target) => needed.max(target.min(ceiling)),
        None => ceiling,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_scales_with_descriptors() {
        let few = setup_open_files(2, None, Some(8), Some(4096))
            .unwrap()
            .unwrap();
        let many = setup_open_files(40, None, Some(8), Some(4096))
            .unwrap()
            .unwrap();
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
            setup_open_files(10, None, Some(8), Some(exact)).unwrap(),
            Some(exact)
        );
        let error = setup_open_files(11, None, Some(8), Some(exact)).unwrap_err();
        assert_eq!(error.inherited_descriptors, 11);
        assert_eq!(error.ceiling, exact);
        assert!(error.needed > exact);
        let message = error.to_string();
        assert!(message.contains("11") && message.contains(&exact.to_string()));
    }

    #[test]
    fn budget_covers_the_highest_target_slot() {
        // Few descriptors, but a sealed payload placed at a high fixed slot.
        let limit = setup_open_files(3, Some(198), Some(64), Some(4096))
            .unwrap()
            .unwrap();
        assert_eq!(limit, 199);
        // A large descriptor peak still wins over a low slot.
        let peak = setup_open_files(200, Some(4), Some(64), Some(4096))
            .unwrap()
            .unwrap();
        assert_eq!(
            peak,
            SETUP_FIXED_DESCRIPTORS + 200 * SETUP_DESCRIPTORS_PER_INHERITED
        );
        let error = setup_open_files(3, Some(198), Some(64), Some(128)).unwrap_err();
        assert_eq!(error.highest_target_slot, Some(198));
        assert_eq!(error.needed, 199);
        assert!(error.to_string().contains("target slots up to 198"));
    }

    #[test]
    fn a_target_without_its_own_limit_inherits_the_ceiling() {
        assert_eq!(
            setup_open_files(3, Some(197), None, Some(4096)).unwrap(),
            Some(4096)
        );
    }

    #[test]
    fn a_target_limit_above_the_need_raises_the_setup_limit() {
        let need = SETUP_FIXED_DESCRIPTORS + 2 * SETUP_DESCRIPTORS_PER_INHERITED;
        assert_eq!(
            setup_open_files(2, None, Some(1024), Some(4096)).unwrap(),
            Some(1024.max(need))
        );
        // A target request above the ceiling is narrowed to it.
        assert_eq!(
            setup_open_files(2, None, Some(10_000), Some(4096)).unwrap(),
            Some(4096)
        );
    }

    #[test]
    fn budget_without_policy_installs_no_limit() {
        assert_eq!(setup_open_files(1000, None, None, None).unwrap(), None);
    }
}
