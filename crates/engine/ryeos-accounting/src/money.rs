//! Fixed-point USD money for the accounting authority path.
//!
//! Authoritative money is integer USD nanos (`1 USD = 1_000_000_000` nanos),
//! never `f64`. Values enter as canonical decimal strings or integer nanos and
//! are rejected — not rounded — when they exceed nine fractional digits at a
//! configuration or RPC boundary. Rounding toward positive infinity exists
//! only for rate × quantity derivation and for provider-reported raw decimals
//! whose signed final-charge contract explicitly permits their scale.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const NANOS_PER_USD: i64 = 1_000_000_000;
const MAX_FRACTION_DIGITS: u32 = 9;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    #[error("usd decimal is not canonical: {0}")]
    NotCanonical(String),
    #[error("usd decimal has more than nine fractional digits: {0}")]
    ExcessScale(String),
    #[error("usd amount overflows the fixed-point range")]
    Overflow,
    #[error("usd amount is negative")]
    Negative,
    #[error("usd amount must be a canonical decimal string, not a JSON number")]
    JsonNumberRejected,
}

/// Non-negative fixed-point USD in integer nanos.
///
/// The inner value is private so a negative amount can never be constructed;
/// arithmetic is checked and never saturates or wraps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UsdNanos(i64);

impl UsdNanos {
    pub const ZERO: UsdNanos = UsdNanos(0);
    pub const MAX: UsdNanos = UsdNanos(i64::MAX);

    pub fn from_nanos(nanos: i64) -> Result<Self, MoneyError> {
        if nanos < 0 {
            return Err(MoneyError::Negative);
        }
        Ok(Self(nanos))
    }

    pub const fn as_nanos(self) -> i64 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Parse a canonical USD decimal string.
    ///
    /// Canonical strings contain only ASCII digits and at most one decimal
    /// point, carry no sign, exponent, separator, padding zero, or
    /// whitespace, and have one to nine fractional digits when a point is
    /// present. More than nine fractional digits is an error, never a
    /// silent rounding.
    pub fn parse_canonical(input: &str) -> Result<Self, MoneyError> {
        let (int_part, frac_part) = match input.split_once('.') {
            Some((i, f)) => (i, Some(f)),
            None => (input, None),
        };

        if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(MoneyError::NotCanonical(input.to_string()));
        }
        if int_part.len() > 1 && int_part.starts_with('0') {
            return Err(MoneyError::NotCanonical(input.to_string()));
        }
        if let Some(frac) = frac_part {
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return Err(MoneyError::NotCanonical(input.to_string()));
            }
            if frac.len() > MAX_FRACTION_DIGITS as usize {
                return Err(MoneyError::ExcessScale(input.to_string()));
            }
        }

        let whole: i64 = int_part.parse().map_err(|_| MoneyError::Overflow)?;
        let whole_nanos = whole
            .checked_mul(NANOS_PER_USD)
            .ok_or(MoneyError::Overflow)?;
        let frac_nanos = match frac_part {
            None => 0,
            Some(frac) => {
                let digits: i64 = frac.parse().expect("ascii digits within nine chars");
                digits * 10_i64.pow(MAX_FRACTION_DIGITS - frac.len() as u32)
            }
        };
        whole_nanos
            .checked_add(frac_nanos)
            .ok_or(MoneyError::Overflow)
            .map(Self)
    }

    /// Parse a provider-reported raw decimal, rounding toward positive
    /// infinity when it carries more than nine fractional digits.
    ///
    /// Returns the enforcement value and whether rounding occurred. Callers
    /// may use this ONLY when the route's signed final-charge contract
    /// explicitly permits the reported scale; the raw text is retained
    /// separately as bounded audit truth. Sign, exponent, and non-digit
    /// characters are still rejected.
    pub fn parse_reported_round_up(input: &str) -> Result<(Self, bool), MoneyError> {
        let (int_part, frac_part) = match input.split_once('.') {
            Some((i, f)) => (i, Some(f)),
            None => (input, None),
        };
        if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(MoneyError::NotCanonical(input.to_string()));
        }
        let frac = frac_part.unwrap_or("");
        if frac_part.is_some() && (frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit())) {
            return Err(MoneyError::NotCanonical(input.to_string()));
        }

        let whole: i64 = int_part.parse().map_err(|_| MoneyError::Overflow)?;
        let whole_nanos = whole
            .checked_mul(NANOS_PER_USD)
            .ok_or(MoneyError::Overflow)?;
        let (kept, dropped) = if frac.len() > MAX_FRACTION_DIGITS as usize {
            frac.split_at(MAX_FRACTION_DIGITS as usize)
        } else {
            (frac, "")
        };
        let kept_nanos = if kept.is_empty() {
            0
        } else {
            let digits: i64 = kept.parse().expect("ascii digits within nine chars");
            digits * 10_i64.pow(MAX_FRACTION_DIGITS - kept.len() as u32)
        };
        let rounded = dropped.bytes().any(|b| b != b'0');
        whole_nanos
            .checked_add(kept_nanos)
            .and_then(|n| n.checked_add(i64::from(rounded)))
            .ok_or(MoneyError::Overflow)
            .map(|n| (Self(n), rounded))
    }

    /// Canonical minimal decimal rendering: no trailing fractional zeros, no
    /// decimal point for whole amounts, never signed, never exponent.
    pub fn to_canonical_string(self) -> String {
        let whole = self.0 / NANOS_PER_USD;
        let frac = self.0 % NANOS_PER_USD;
        if frac == 0 {
            return whole.to_string();
        }
        let mut frac_str = format!("{frac:09}");
        while frac_str.ends_with('0') {
            frac_str.pop();
        }
        format!("{whole}.{frac_str}")
    }

    pub fn checked_add(self, other: UsdNanos) -> Result<UsdNanos, MoneyError> {
        self.0
            .checked_add(other.0)
            .ok_or(MoneyError::Overflow)
            .map(UsdNanos)
    }

    pub fn checked_sub(self, other: UsdNanos) -> Result<UsdNanos, MoneyError> {
        if other.0 > self.0 {
            return Err(MoneyError::Negative);
        }
        Ok(UsdNanos(self.0 - other.0))
    }

    /// `rate_per_million × units / 1_000_000`, rounded toward positive
    /// infinity, computed in checked `i128` and required to fit the
    /// fixed-point range before commit.
    pub fn rate_per_million_mul_units_round_up(
        rate_per_million: UsdNanos,
        units: u64,
    ) -> Result<UsdNanos, MoneyError> {
        const UNITS_PER_RATE: i128 = 1_000_000;
        let product = i128::from(rate_per_million.0)
            .checked_mul(i128::from(units))
            .ok_or(MoneyError::Overflow)?;
        let nanos = (product + (UNITS_PER_RATE - 1)) / UNITS_PER_RATE;
        i64::try_from(nanos)
            .map_err(|_| MoneyError::Overflow)
            .map(UsdNanos)
    }

    /// One-way presentation value. Never parse this back into authority.
    pub fn display_usd_lossy(self) -> f64 {
        self.0 as f64 / NANOS_PER_USD as f64
    }
}

impl Serialize for UsdNanos {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_canonical_string())
    }
}

impl<'de> Deserialize<'de> for UsdNanos {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(s) => UsdNanos::parse_canonical(&s).map_err(D::Error::custom),
            serde_json::Value::Number(_) => Err(D::Error::custom(MoneyError::JsonNumberRejected)),
            other => Err(D::Error::custom(format!(
                "usd amount must be a canonical decimal string, got {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_decimals() {
        assert_eq!(UsdNanos::parse_canonical("0").unwrap().as_nanos(), 0);
        assert_eq!(
            UsdNanos::parse_canonical("1").unwrap().as_nanos(),
            NANOS_PER_USD
        );
        assert_eq!(
            UsdNanos::parse_canonical("0.03").unwrap().as_nanos(),
            30_000_000
        );
        assert_eq!(
            UsdNanos::parse_canonical("0.000000001").unwrap().as_nanos(),
            1
        );
        assert_eq!(
            UsdNanos::parse_canonical("12.5").unwrap().as_nanos(),
            12_500_000_000
        );
        // Trailing zeros inside nine digits are exact and accepted.
        assert_eq!(
            UsdNanos::parse_canonical("0.50").unwrap().as_nanos(),
            500_000_000
        );
    }

    #[test]
    fn rejects_noncanonical_decimals() {
        for bad in [
            "", ".", ".5", "5.", "-1", "+1", "1e3", "1E3", "1_000", " 1", "1 ", "01", "00.5", "1.",
            "0x10", "NaN", "1,5",
        ] {
            assert!(
                UsdNanos::parse_canonical(bad).is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_excess_scale_without_rounding() {
        assert_eq!(
            UsdNanos::parse_canonical("0.0000000001"),
            Err(MoneyError::ExcessScale("0.0000000001".to_string()))
        );
    }

    #[test]
    fn rejects_overflow() {
        assert_eq!(
            UsdNanos::parse_canonical("99999999999999999999"),
            Err(MoneyError::Overflow)
        );
        // i64::MAX nanos ≈ 9_223_372_036.854775807 USD
        assert!(UsdNanos::parse_canonical("9223372036.854775807").is_ok());
        assert_eq!(
            UsdNanos::parse_canonical("9223372036.854775808"),
            Err(MoneyError::Overflow)
        );
    }

    #[test]
    fn reported_round_up_rounds_toward_positive_infinity() {
        let (v, rounded) = UsdNanos::parse_reported_round_up("0.0000000001").unwrap();
        assert_eq!(v.as_nanos(), 1);
        assert!(rounded);
        let (v, rounded) = UsdNanos::parse_reported_round_up("0.00000000010000").unwrap();
        assert_eq!(v.as_nanos(), 1);
        assert!(rounded);
        let (v, rounded) = UsdNanos::parse_reported_round_up("0.0000000010").unwrap();
        assert_eq!(v.as_nanos(), 1);
        assert!(!rounded);
        assert!(UsdNanos::parse_reported_round_up("-0.1").is_err());
        assert!(UsdNanos::parse_reported_round_up("1e-3").is_err());
    }

    #[test]
    fn canonical_rendering_is_minimal() {
        for (nanos, expected) in [
            (0, "0"),
            (1, "0.000000001"),
            (30_000_000, "0.03"),
            (NANOS_PER_USD, "1"),
            (12_500_000_000, "12.5"),
        ] {
            assert_eq!(
                UsdNanos::from_nanos(nanos).unwrap().to_canonical_string(),
                expected
            );
        }
    }

    #[test]
    fn parse_render_round_trip() {
        for s in [
            "0",
            "0.000000001",
            "0.03",
            "1",
            "12.5",
            "9223372036.854775807",
        ] {
            let v = UsdNanos::parse_canonical(s).unwrap();
            assert_eq!(v.to_canonical_string(), s);
            assert_eq!(
                UsdNanos::parse_canonical(&v.to_canonical_string()).unwrap(),
                v
            );
        }
    }

    #[test]
    fn checked_arithmetic() {
        let a = UsdNanos::parse_canonical("1.5").unwrap();
        let b = UsdNanos::parse_canonical("0.5").unwrap();
        assert_eq!(a.checked_add(b).unwrap().to_canonical_string(), "2");
        assert_eq!(a.checked_sub(b).unwrap().to_canonical_string(), "1");
        assert_eq!(b.checked_sub(a), Err(MoneyError::Negative));
        assert_eq!(UsdNanos::MAX.checked_add(b), Err(MoneyError::Overflow));
    }

    #[test]
    fn rate_mul_rounds_up_in_i128() {
        // $3 per million tokens × 1 token = 3000 nanos exactly.
        let rate = UsdNanos::parse_canonical("3").unwrap();
        assert_eq!(
            UsdNanos::rate_per_million_mul_units_round_up(rate, 1)
                .unwrap()
                .as_nanos(),
            3_000
        );
        // 1 nano per million × 1 unit rounds up to 1 nano, never to zero.
        let tiny = UsdNanos::from_nanos(1).unwrap();
        assert_eq!(
            UsdNanos::rate_per_million_mul_units_round_up(tiny, 1)
                .unwrap()
                .as_nanos(),
            1
        );
        // Exact multiples do not round.
        assert_eq!(
            UsdNanos::rate_per_million_mul_units_round_up(tiny, 1_000_000)
                .unwrap()
                .as_nanos(),
            1
        );
        // Overflow is an error, not saturation.
        assert_eq!(
            UsdNanos::rate_per_million_mul_units_round_up(UsdNanos::MAX, u64::MAX),
            Err(MoneyError::Overflow)
        );
    }

    #[test]
    fn serde_is_canonical_string_only() {
        let v: UsdNanos = serde_json::from_str("\"0.03\"").unwrap();
        assert_eq!(v.as_nanos(), 30_000_000);
        assert_eq!(serde_json::to_string(&v).unwrap(), "\"0.03\"");
        assert!(serde_json::from_str::<UsdNanos>("0.03").is_err());
        assert!(serde_json::from_str::<UsdNanos>("3").is_err());
        assert!(serde_json::from_str::<UsdNanos>("null").is_err());
        assert!(serde_json::from_str::<UsdNanos>("\"-1\"").is_err());
    }
}
