//! Node-owned limits on what the node publishes to strangers.
//!
//! `daily_bytes` is the node-wide ceiling on bytes the `/published` routes
//! serve over a rolling 24 hours, counted across every publication and every
//! reader. It bounds the node's bill, not its availability: a stranger can use
//! up the day's cap. `max_lifetime_days` is how far ahead a publication's
//! `not_after` may be; code adds a hard maximum that node data cannot raise.
//! `max_in_flight` bounds how many `/published` requests are worked at once.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "publication";

/// Largest in-flight limit node data may set.
pub const MAX_IN_FLIGHT_CEILING: u32 = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationPolicy {
    pub schema: u32,
    pub daily_bytes: u64,
    pub max_lifetime_days: u32,
    /// Most `/published` requests the node works on at once, node-wide. A
    /// request beyond it is answered `publication_busy` before any read.
    pub max_in_flight: u32,
}

impl PublicationPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("node publication policy schema is not current");
        }
        if self.daily_bytes == 0 {
            bail!("node publication policy daily_bytes must be positive");
        }
        if self.max_lifetime_days == 0
            || self.max_lifetime_days > ryeos_engine::trust::PUBLICATION_HARD_MAX_LIFETIME_DAYS
        {
            bail!(
                "node publication policy max_lifetime_days must be 1 to {}",
                ryeos_engine::trust::PUBLICATION_HARD_MAX_LIFETIME_DAYS
            );
        }
        if self.max_in_flight == 0 || self.max_in_flight > MAX_IN_FLIGHT_CEILING {
            bail!("node publication policy max_in_flight must be 1 to {MAX_IN_FLIGHT_CEILING}");
        }
        Ok(())
    }

    /// The policy every shipped init profile carries: one gibibyte a day and a
    /// 90-day lifetime. Builds exact test snapshots only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn as_shipped() -> Self {
        Self {
            schema: 1,
            daily_bytes: 1 << 30,
            max_lifetime_days: 90,
            max_in_flight: 16,
        }
    }
}

impl TypedNodePolicy for PublicationPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct PublicationPolicySection;

impl NodePolicySection for PublicationPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: PublicationPolicy =
            serde_json::from_value(body.clone()).context("parse node publication policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_policy_validates_and_bad_ones_do_not() {
        PublicationPolicy::as_shipped().validate().unwrap();
        let mut zero = PublicationPolicy::as_shipped();
        zero.daily_bytes = 0;
        assert!(zero.validate().is_err());
        let mut far = PublicationPolicy::as_shipped();
        far.max_lifetime_days = ryeos_engine::trust::PUBLICATION_HARD_MAX_LIFETIME_DAYS + 1;
        assert!(far.validate().is_err());
        let mut busy = PublicationPolicy::as_shipped();
        busy.max_in_flight = 0;
        assert!(busy.validate().is_err());
        let mut none = PublicationPolicy::as_shipped();
        none.max_lifetime_days = 0;
        assert!(none.validate().is_err());
    }
}
