use serde::{Deserialize, Serialize};

use crate::protocol_vocabulary::error::VocabularyError;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleMode {
    /// Daemon spawns child, awaits exit, returns ExecutionCompletion.
    /// Tool-style.
    Oneshot,
    /// Daemon spawns child with a callback channel, child reports state
    /// via the channel, daemon awaits final result.
    /// Runtime-style.
    Managed,
    /// Caller may choose launch_mode: detached or inline. Daemon returns
    /// immediately on detached, polls on inline.
    DetachedOk,
}

/// Capability matrix: (LifecycleMode, allows_detached).
///
/// | LifecycleMode | allows_detached MUST be |
/// | ------------- | ----------------------- |
/// | Oneshot       | false                   |
/// | Managed       | false                   |
/// | DetachedOk    | true                    |
pub fn is_compatible_lifecycle_detached(
    mode: LifecycleMode,
    allows_detached: bool,
) -> Result<(), VocabularyError> {
    let expected = matches!(mode, LifecycleMode::DetachedOk);
    if expected != allows_detached {
        return Err(VocabularyError::LifecycleDetachedMismatch {
            lifecycle: format!("{:?}", mode),
            expected,
            actual: allows_detached,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_variants() {
        for mode in [
            LifecycleMode::Oneshot,
            LifecycleMode::Managed,
            LifecycleMode::DetachedOk,
        ] {
            let yaml = serde_yaml::to_string(&mode).unwrap();
            let parsed: LifecycleMode = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(parsed, mode);
        }
    }

    #[test]
    fn reject_unknown() {
        let err = serde_yaml::from_str::<LifecycleMode>("unknown");
        assert!(err.is_err());
    }

    #[test]
    fn lifecycle_detached_matrix() {
        let cases: Vec<(LifecycleMode, bool, bool)> = vec![
            (LifecycleMode::Oneshot, false, true),
            (LifecycleMode::Oneshot, true, false),
            (LifecycleMode::Managed, false, true),
            (LifecycleMode::Managed, true, false),
            (LifecycleMode::DetachedOk, true, true),
            (LifecycleMode::DetachedOk, false, false),
        ];
        for (mode, detached, compatible) in cases {
            let result = is_compatible_lifecycle_detached(mode, detached);
            assert_eq!(
                result.is_ok(),
                compatible,
                "({:?}, allows_detached={}) expected compatible={}, got {:?}",
                mode, detached, compatible, result
            );
        }
    }
}
