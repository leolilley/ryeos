//! Pure policy parsing; evaluation remains scheduler-owned.

use anyhow::Result;

/// All four misfire policies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MisfirePolicy {
    Skip,
    FireOnceNow,
    CatchUpBounded(usize),
    CatchUpWithinSecs(u64),
}

pub fn parse_misfire_policy(raw: &str) -> Result<MisfirePolicy> {
    match raw {
        "skip" => Ok(MisfirePolicy::Skip),
        "fire_once_now" => Ok(MisfirePolicy::FireOnceNow),
        s if s.starts_with("catch_up_bounded:") => {
            let count = s
                .strip_prefix("catch_up_bounded:")
                .expect("prefix checked")
                .parse::<usize>()
                .map_err(|_| anyhow::anyhow!("invalid misfire_policy: {raw}"))?;
            Ok(MisfirePolicy::CatchUpBounded(count))
        }
        s if s.starts_with("catch_up_within_secs:") => {
            let seconds = s
                .strip_prefix("catch_up_within_secs:")
                .expect("prefix checked")
                .parse::<u64>()
                .map_err(|_| anyhow::anyhow!("invalid misfire_policy: {raw}"))?;
            if seconds > (i64::MAX as u64) / 1_000 {
                anyhow::bail!("misfire_policy window is too large: {raw}");
            }
            Ok(MisfirePolicy::CatchUpWithinSecs(seconds))
        }
        _ => anyhow::bail!("invalid misfire_policy: {raw}"),
    }
}

/// All three overlap policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlapPolicy {
    Allow,
    Skip,
    CancelPrevious,
}

pub fn parse_overlap_policy(raw: &str) -> Result<OverlapPolicy> {
    match raw {
        "allow" => Ok(OverlapPolicy::Allow),
        "cancel_previous" => Ok(OverlapPolicy::CancelPrevious),
        "skip" => Ok(OverlapPolicy::Skip),
        _ => anyhow::bail!("invalid overlap_policy: {raw}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn misfire_vocabulary_preserves_explicit_zero_bounds() {
        assert_eq!(
            parse_misfire_policy("catch_up_bounded:0").unwrap(),
            MisfirePolicy::CatchUpBounded(0)
        );
        assert_eq!(
            parse_misfire_policy("catch_up_within_secs:0").unwrap(),
            MisfirePolicy::CatchUpWithinSecs(0)
        );
        assert!(parse_misfire_policy("catch_up_bounded:-1").is_err());
        assert!(parse_misfire_policy("catch_up_within_secs:9223372036854776").is_err());
        assert!(parse_misfire_policy("").is_err());
    }

    #[test]
    fn overlap_requires_exact_explicit_policy() {
        assert_eq!(parse_overlap_policy("allow").unwrap(), OverlapPolicy::Allow);
        assert_eq!(parse_overlap_policy("skip").unwrap(), OverlapPolicy::Skip);
        assert_eq!(
            parse_overlap_policy("cancel_previous").unwrap(),
            OverlapPolicy::CancelPrevious
        );
        assert!(parse_overlap_policy(" skip").is_err());
        assert!(parse_overlap_policy("").is_err());
    }
}
