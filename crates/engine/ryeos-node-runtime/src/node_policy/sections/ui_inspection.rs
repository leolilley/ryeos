//! Node-owned read budgets for UI thread inspection.
//!
//! Inspection answers from signed history, and these budgets decide how much
//! of one thread's history a single call may scan before it reports the answer
//! as not proven. They are an operator trade-off between latency and
//! completeness, not a protocol limit shared with a client decoder. The policy
//! has no daemon fallback: registration makes the section a required member of
//! every complete node-policy generation.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "ui_inspection";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiInspectionPolicy {
    pub schema: u32,
    /// Most events one inspection call returns, whatever the caller asks.
    pub max_event_limit: u32,
    /// Most events one signed-history page of the facet scan reads.
    pub max_facet_page_events: u32,
    /// Most record bytes one signed-history page of the facet scan reads.
    pub max_facet_page_bytes: u64,
    /// Most pages the facet scan of one thread reads before it reports the
    /// facet map as not proven.
    pub max_facet_pages_per_thread: u32,
    /// Most pages the facet scans of one execution tree read together.
    pub max_facet_pages_total: u32,
}

impl UiInspectionPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("UI inspection node policy schema is not current");
        }
        for (name, value) in [
            ("max_event_limit", u64::from(self.max_event_limit)),
            (
                "max_facet_page_events",
                u64::from(self.max_facet_page_events),
            ),
            ("max_facet_page_bytes", self.max_facet_page_bytes),
            (
                "max_facet_pages_per_thread",
                u64::from(self.max_facet_pages_per_thread),
            ),
            (
                "max_facet_pages_total",
                u64::from(self.max_facet_pages_total),
            ),
        ] {
            if value == 0 {
                bail!("UI inspection {name} must be non-zero");
            }
        }
        if self.max_facet_pages_per_thread > self.max_facet_pages_total {
            bail!("UI inspection max_facet_pages_per_thread exceeds max_facet_pages_total");
        }
        Ok(())
    }
}

impl TypedNodePolicy for UiInspectionPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct UiInspectionPolicySection;

impl NodePolicySection for UiInspectionPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: UiInspectionPolicy =
            serde_json::from_value(body.clone()).context("parse UI inspection node policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> NodePolicyContext {
        NodePolicyContext {
            section: SECTION_NAME.to_owned(),
            source_file: "/node/policies/ui_inspection.yaml".into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    fn current_body() -> Value {
        serde_json::json!({
            "schema": 1,
            "max_event_limit": 500,
            "max_facet_page_events": 64,
            "max_facet_page_bytes": 524288,
            "max_facet_pages_per_thread": 8,
            "max_facet_pages_total": 64
        })
    }

    #[test]
    fn parses_exact_current_budgets() {
        let parsed = UiInspectionPolicySection
            .parse(&context(), &current_body())
            .unwrap();
        let policy = parsed
            .as_any()
            .downcast_ref::<UiInspectionPolicy>()
            .unwrap();
        assert_eq!(policy.max_event_limit, 500);
        assert_eq!(policy.max_facet_page_events, 64);
        assert_eq!(policy.max_facet_page_bytes, 524288);
        assert_eq!(policy.max_facet_pages_per_thread, 8);
        assert_eq!(policy.max_facet_pages_total, 64);
    }

    #[test]
    fn rejects_absent_zero_stale_unknown_and_inverted_budgets() {
        let original = current_body();
        let mut bad = Vec::new();
        for name in [
            "schema",
            "max_event_limit",
            "max_facet_page_events",
            "max_facet_page_bytes",
            "max_facet_pages_per_thread",
            "max_facet_pages_total",
        ] {
            let mut missing = original.clone();
            missing.as_object_mut().unwrap().remove(name);
            bad.push(missing);
        }
        for name in [
            "max_event_limit",
            "max_facet_page_events",
            "max_facet_page_bytes",
            "max_facet_pages_per_thread",
            "max_facet_pages_total",
        ] {
            let mut zero = original.clone();
            zero[name] = serde_json::json!(0);
            bad.push(zero);
        }
        for schema in [0, 2] {
            let mut stale = original.clone();
            stale["schema"] = serde_json::json!(schema);
            bad.push(stale);
        }
        let mut unknown = original.clone();
        unknown["fallback"] = serde_json::json!(1);
        bad.push(unknown);
        let mut inverted = original;
        inverted["max_facet_pages_per_thread"] = serde_json::json!(65);
        bad.push(inverted);
        for body in bad {
            assert!(UiInspectionPolicySection.parse(&context(), &body).is_err());
        }
    }
}
