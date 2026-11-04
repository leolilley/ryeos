//! How far a guest's wall clock may disagree with the controller's when the
//! guest checks a controller-written absolute deadline for freshness.
//!
//! The controller owns every window and enforces it on its own clock; a guest
//! never uses wall-clock comparison to bound a window. A freshness check on
//! the guest ("do not start something the controller's deadline has already
//! passed") is the one place the two clocks meet, so the signed contract
//! declares how much disagreement it accepts and a refusal names what the
//! guest observed.

use anyhow::{Result, ensure};
use std::fmt::Display;

/// A tolerance a node's policy may declare in the transport contracts it
/// composes. Composition leaves it unset unless policy supplies a value, so a
/// composed contract's bytes and digest do not change; the guest then applies
/// the same value as its fallback.
pub const DEFAULT_MAX_CLOCK_SKEW_MS: u32 = 60_000;

/// Tolerance applied when a signed contract declares none: contract documents
/// that predate the field, whose bytes must not change.
pub const FALLBACK_MAX_CLOCK_SKEW_MS: u32 = DEFAULT_MAX_CLOCK_SKEW_MS;

/// Largest tolerance a contract may declare.
pub const MAXIMUM_DECLARED_CLOCK_SKEW_MS: u32 = 600_000;

/// The tolerance in force for a contract that may or may not declare one.
pub fn effective_max_clock_skew_ms(declared: Option<u32>) -> u32 {
    declared.unwrap_or(FALLBACK_MAX_CLOCK_SKEW_MS)
}

pub fn validate_declared_max_clock_skew_ms(declared: Option<u32>) -> Result<()> {
    ensure!(
        declared.is_none_or(|ms| ms <= MAXIMUM_DECLARED_CLOCK_SKEW_MS),
        "declared clock skew tolerance exceeds its bound"
    );
    Ok(())
}

/// Refuse when the guest's wall clock stands at or beyond `deadline_ms` plus
/// the declared tolerance. The refusal names the observed difference, the
/// tolerance and what the guest knows about its own clock.
pub fn require_fresh_before(
    subject: &str,
    deadline_ms: i64,
    guest_now_ms: i64,
    max_clock_skew_ms: u32,
    guest_clock_trust: &dyn Display,
) -> Result<()> {
    let limit = deadline_ms.saturating_add(i64::from(max_clock_skew_ms));
    ensure!(
        guest_now_ms > 0 && guest_now_ms < limit,
        "guest clock is {} s past the controller's {subject}; declared skew tolerance {} s; \
         guest clock trust: {guest_clock_trust}",
        guest_now_ms.saturating_sub(deadline_ms) / 1_000,
        max_clock_skew_ms / 1_000,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_accepts_up_to_the_tolerance_past_the_deadline() {
        let trust = "synchronized (maximum error 12 us)";
        require_fresh_before("attachment deadline", 1_000_000, 999_000, 60_000, &trust).unwrap();
        // Past the deadline but within the declared tolerance.
        require_fresh_before("attachment deadline", 1_000_000, 1_059_999, 60_000, &trust).unwrap();
        // A guest clock behind the controller's is always fresh.
        require_fresh_before("attachment deadline", 1_000_000, 400_000, 60_000, &trust).unwrap();
    }

    #[test]
    fn freshness_refusal_names_difference_tolerance_and_trust() {
        let error = require_fresh_before(
            "attachment deadline",
            1_000_000,
            1_094_000,
            60_000,
            &"unsynchronized",
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "guest clock is 94 s past the controller's attachment deadline; \
             declared skew tolerance 60 s; guest clock trust: unsynchronized"
        );
        assert!(
            require_fresh_before(
                "attachment deadline",
                1_000_000,
                1_060_000,
                60_000,
                &"unknown"
            )
            .is_err()
        );
    }

    #[test]
    fn declared_tolerance_is_bounded_and_defaults_when_absent() {
        assert_eq!(
            effective_max_clock_skew_ms(None),
            FALLBACK_MAX_CLOCK_SKEW_MS
        );
        assert_eq!(effective_max_clock_skew_ms(Some(5)), 5);
        validate_declared_max_clock_skew_ms(None).unwrap();
        validate_declared_max_clock_skew_ms(Some(MAXIMUM_DECLARED_CLOCK_SKEW_MS)).unwrap();
        assert!(
            validate_declared_max_clock_skew_ms(Some(MAXIMUM_DECLARED_CLOCK_SKEW_MS + 1)).is_err()
        );
    }
}
