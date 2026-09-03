//! Kind-neutral admission projection for financial authority.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ProviderAccountingAuthority, SpendBoundAuthority};

pub const FINANCIAL_AUTHORITY_KIND: &str = "accounting";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedFinancialAuthority {
    pub kind: String,
    pub authority: Value,
    pub authority_digest: String,
    pub spend_bound: SpendBoundClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SpendBoundClass {
    Paid,
    ExplicitlyFree,
    AdvisoryOnly,
}

impl SpendBoundClass {
    pub const fn hard_spend_eligible(self) -> bool {
        matches!(self, Self::Paid | Self::ExplicitlyFree)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Paid => "paid",
            Self::ExplicitlyFree => "explicitly_free",
            Self::AdvisoryOnly => "advisory_only",
        }
    }
}

/// Strictly decode the current accounting contract once and publish only its
/// mechanical admission facts to generic launch code.
pub fn admit_financial_authority(value: Value) -> anyhow::Result<AdmittedFinancialAuthority> {
    let decoded: ProviderAccountingAuthority = serde_json::from_value(value)?;
    decoded.validate().map_err(anyhow::Error::msg)?;
    let spend_bound = match &decoded.spend_bound {
        SpendBoundAuthority::Paid { .. } => SpendBoundClass::Paid,
        SpendBoundAuthority::ExplicitlyFree { .. } => SpendBoundClass::ExplicitlyFree,
        SpendBoundAuthority::AdvisoryOnly => SpendBoundClass::AdvisoryOnly,
    };
    let authority = serde_json::to_value(&decoded)?;
    let canonical = lillux::canonical_json(&authority)?;
    Ok(AdmittedFinancialAuthority {
        kind: FINANCIAL_AUTHORITY_KIND.to_owned(),
        authority,
        authority_digest: lillux::sha256_hex(canonical.as_bytes()),
        spend_bound,
    })
}
