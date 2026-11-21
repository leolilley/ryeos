//! Start-up proof of the kernel features the node's process control needs.
//!
//! A kernel that lacks one of them would otherwise fail later and lazily, on
//! the first runtime attachment or the first exact process-group signal. The
//! node proves both here and refuses to start, naming the missing feature and
//! the Linux release that introduced it.

use std::fmt;

/// A kernel feature the node cannot run without. The set is closed: each
/// entry is a distinct kernel mechanism with its own introduction release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelFeature {
    /// `SO_PEERPIDFD` on Unix sockets, used to authenticate a runtime peer.
    SoPeerPidfd,
    /// `PIDFD_SIGNAL_PROCESS_GROUP` in `pidfd_send_signal`, used for exact
    /// process-group delivery.
    PidfdSignalProcessGroup,
}

impl KernelFeature {
    pub const fn name(self) -> &'static str {
        match self {
            Self::SoPeerPidfd => "so_peerpidfd",
            Self::PidfdSignalProcessGroup => "pidfd_signal_process_group",
        }
    }

    /// The Linux release that introduced the feature.
    pub const fn floor(self) -> &'static str {
        match self {
            Self::SoPeerPidfd => "6.5",
            Self::PidfdSignalProcessGroup => "6.9",
        }
    }
}

/// Fixed-vocabulary start refusal: the code is always `kernel_feature_missing`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct KernelFeatureMissing {
    pub feature: KernelFeature,
    pub reason: String,
}

impl KernelFeatureMissing {
    pub const CODE: &'static str = "kernel_feature_missing";
}

impl fmt::Display for KernelFeatureMissing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: feature {} requires Linux {} or newer: {}",
            Self::CODE,
            self.feature.name(),
            self.feature.floor(),
            self.reason
        )
    }
}

/// Map the two probe results to the refusal, peer identity first.
pub fn check_kernel_floor(
    peer_pidfd: Result<(), String>,
    group_signal: Result<(), String>,
) -> Result<(), KernelFeatureMissing> {
    peer_pidfd.map_err(|reason| KernelFeatureMissing {
        feature: KernelFeature::SoPeerPidfd,
        reason,
    })?;
    group_signal.map_err(|reason| KernelFeatureMissing {
        feature: KernelFeature::PidfdSignalProcessGroup,
        reason,
    })
}

/// Run both real probes. The group probe needs the process to lead its own
/// session, so this runs after the session is established.
pub fn require_kernel_floor() -> Result<(), KernelFeatureMissing> {
    check_kernel_floor(
        lillux::local_ipc::validate_peer_process_control_support().map_err(|e| format!("{e:#}")),
        lillux::process_control::prepare_process_group_controller(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_probes_passing_admits() {
        assert_eq!(check_kernel_floor(Ok(()), Ok(())), Ok(()));
    }

    #[test]
    fn missing_peer_pidfd_names_feature_and_floor() {
        let error = check_kernel_floor(Err("ENOPROTOOPT".into()), Ok(())).unwrap_err();
        assert_eq!(error.feature, KernelFeature::SoPeerPidfd);
        assert_eq!(
            error.to_string(),
            "kernel_feature_missing: feature so_peerpidfd requires Linux 6.5 or newer: ENOPROTOOPT"
        );
    }

    #[test]
    fn missing_group_signal_names_feature_and_floor() {
        let error = check_kernel_floor(Ok(()), Err("EINVAL".into())).unwrap_err();
        assert_eq!(error.feature, KernelFeature::PidfdSignalProcessGroup);
        assert_eq!(
            error.to_string(),
            "kernel_feature_missing: feature pidfd_signal_process_group requires Linux 6.9 or newer: EINVAL"
        );
    }

    #[test]
    fn peer_failure_is_reported_before_group_failure() {
        let error = check_kernel_floor(Err("a".into()), Err("b".into())).unwrap_err();
        assert_eq!(error.feature, KernelFeature::SoPeerPidfd);
    }

    #[test]
    fn refusal_survives_anyhow_as_the_typed_error() {
        let error =
            anyhow::Error::from(check_kernel_floor(Ok(()), Err("EINVAL".into())).unwrap_err());
        assert!(error.downcast_ref::<KernelFeatureMissing>().is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_host_kernel_meets_the_floor() {
        // The test host is a supported kernel; a failure here is the refusal
        // the daemon would print at start.
        let peer = lillux::local_ipc::validate_peer_process_control_support()
            .map_err(|e| format!("{e:#}"));
        assert_eq!(check_kernel_floor(peer, Ok(())), Ok(()));
    }
}
