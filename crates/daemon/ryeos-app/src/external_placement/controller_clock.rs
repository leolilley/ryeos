//! The controller's own wall-clock trust, checked before it issues the
//! absolute-time windows of an external execution.
//!
//! The controller is the authority on every window and enforces each on its
//! own clock, so a node whose clock is wrong issues windows that are wrong for
//! everyone. Node execution policy may require a synchronized clock with a
//! bounded error; without such a requirement the node issues windows whatever
//! its clock says and only reports the trust (`ryeos node doctor`).

use anyhow::Result;
use ryeos_node_runtime::node_policy::sections::execution::{
    ExternalWallClockRequirement, NodeExecutionAdmissionPolicy,
};

use crate::state::AppState;

/// Refuse to issue external execution windows when node policy requires a
/// trustworthy wall clock and this host cannot show one.
pub(crate) fn require_for_new_windows(state: &AppState) -> Result<()> {
    let requirement = state
        .node_policy
        .require::<NodeExecutionAdmissionPolicy>()?
        .external_wall_clock_requirement;
    require_with(requirement.as_ref(), lillux::time::wall_clock_trust)
}

/// The requirement judged against a trust reading, sampled only when a
/// requirement exists.
pub(crate) fn require_with(
    requirement: Option<&ExternalWallClockRequirement>,
    trust: impl FnOnce() -> lillux::time::WallClockTrust,
) -> Result<()> {
    match requirement {
        Some(requirement) => requirement.require(&trust()),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::time::WallClockTrust;

    #[test]
    fn without_a_requirement_the_clock_is_never_consulted() {
        require_with(None, || panic!("trust sampled without a requirement")).unwrap();
    }

    #[test]
    fn a_requirement_refuses_an_unsatisfying_clock_and_names_the_cause() {
        let requirement = ExternalWallClockRequirement {
            max_error_us: 100_000,
        };
        require_with(Some(&requirement), || WallClockTrust::Synced {
            maximum_error_us: 20,
            estimated_error_us: 5,
        })
        .unwrap();
        let refusal = require_with(Some(&requirement), || WallClockTrust::Unsynchronized)
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("wall clock is unsynchronized") && refusal.contains("100000 us"),
            "{refusal}"
        );
    }
}
