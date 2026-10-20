//! Node-owned resource limits for authenticated UI browser sessions.
//!
//! The UI session store enforces this compiled policy when it admits retained
//! binding attachments. The policy has no daemon fallback: registration makes
//! the section a required member of every complete node-policy generation.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "ui_browser_sessions";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBrowserSessionPolicy {
    pub schema: u32,
    pub max_session_bootstrap_bytes: u64,
    pub max_live_binding_attachments_per_session: u32,
    pub max_presentations_per_session: u32,
    pub max_surface_instances_per_presentation: u32,
    pub shared_presentation: SharedPresentationPolicyLimits,
}

/// Admission bounds for the signed shared declaration; not renderer defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedPresentationPolicyLimits {
    pub max_declaration_bytes: u64,
    pub max_dialogs: u32,
    pub max_initial_surfaces: u32,
    pub max_label_bytes: u32,
    pub max_mount_id_bytes: u32,
}

impl UiBrowserSessionPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 3 {
            bail!("UI browser-session node policy schema is not current");
        }
        if self.max_live_binding_attachments_per_session == 0 {
            bail!("UI browser-session max_live_binding_attachments_per_session must be non-zero");
        }
        for (name, value) in [
            (
                "max_session_bootstrap_bytes",
                self.max_session_bootstrap_bytes,
            ),
            (
                "max_presentations_per_session",
                u64::from(self.max_presentations_per_session),
            ),
            (
                "max_surface_instances_per_presentation",
                u64::from(self.max_surface_instances_per_presentation),
            ),
            (
                "shared_presentation.max_declaration_bytes",
                self.shared_presentation.max_declaration_bytes,
            ),
            (
                "shared_presentation.max_dialogs",
                u64::from(self.shared_presentation.max_dialogs),
            ),
            (
                "shared_presentation.max_label_bytes",
                u64::from(self.shared_presentation.max_label_bytes),
            ),
            (
                "shared_presentation.max_mount_id_bytes",
                u64::from(self.shared_presentation.max_mount_id_bytes),
            ),
        ] {
            if value == 0 {
                bail!("UI browser-session {name} must be non-zero");
            }
        }
        if self.shared_presentation.max_initial_surfaces
            > self.max_surface_instances_per_presentation
        {
            bail!("UI browser-session initial Surfaces exceed the presentation Surface limit");
        }
        Ok(())
    }
}

impl TypedNodePolicy for UiBrowserSessionPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct UiBrowserSessionPolicySection;

impl NodePolicySection for UiBrowserSessionPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: UiBrowserSessionPolicy =
            serde_json::from_value(body.clone()).context("parse UI browser-session node policy")?;
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
            source_file: "/node/policies/ui_browser_sessions.yaml".into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    #[test]
    fn parses_exact_current_limits() {
        let parsed = UiBrowserSessionPolicySection
            .parse(&context(), &current_body())
            .unwrap();
        let policy = parsed
            .as_any()
            .downcast_ref::<UiBrowserSessionPolicy>()
            .unwrap();
        assert_eq!(policy.max_live_binding_attachments_per_session, 16);
        assert_eq!(policy.max_session_bootstrap_bytes, 4194304);
        assert_eq!(policy.shared_presentation.max_declaration_bytes, 65536);
    }

    fn current_body() -> Value {
        serde_json::json!({
            "schema": 3,
            "max_session_bootstrap_bytes": 4194304,
            "max_live_binding_attachments_per_session": 16,
            "max_presentations_per_session": 16,
            "max_surface_instances_per_presentation": 16,
            "shared_presentation": {
                "max_declaration_bytes": 65536,
                "max_dialogs": 16,
                "max_initial_surfaces": 16,
                "max_label_bytes": 128,
                "max_mount_id_bytes": 64
            }
        })
    }

    #[test]
    fn rejects_absent_zero_stale_and_unknown_limits() {
        let original = current_body();
        let mut bad = Vec::new();
        for name in [
            "schema",
            "max_session_bootstrap_bytes",
            "max_live_binding_attachments_per_session",
            "max_presentations_per_session",
            "max_surface_instances_per_presentation",
            "shared_presentation",
        ] {
            let mut missing = original.clone();
            missing.as_object_mut().unwrap().remove(name);
            bad.push(missing);
        }
        for name in [
            "max_session_bootstrap_bytes",
            "max_live_binding_attachments_per_session",
            "max_presentations_per_session",
            "max_surface_instances_per_presentation",
        ] {
            let mut zero = original.clone();
            zero[name] = serde_json::json!(0);
            bad.push(zero);
        }
        for name in [
            "max_declaration_bytes",
            "max_dialogs",
            "max_label_bytes",
            "max_mount_id_bytes",
            "max_initial_surfaces",
        ] {
            let mut missing = original.clone();
            missing["shared_presentation"]
                .as_object_mut()
                .unwrap()
                .remove(name);
            bad.push(missing);
            if name != "max_initial_surfaces" {
                let mut zero = original.clone();
                zero["shared_presentation"][name] = serde_json::json!(0);
                bad.push(zero);
            }
        }
        for predecessor in [1, 2] {
            let mut stale = original.clone();
            stale["schema"] = serde_json::json!(predecessor);
            bad.push(stale);
        }
        let mut unknown = original.clone();
        unknown["fallback"] = serde_json::json!(1);
        bad.push(unknown);
        let mut nested_unknown = original.clone();
        nested_unknown["shared_presentation"]["fallback"] = serde_json::json!(1);
        bad.push(nested_unknown);
        let mut excess = original;
        excess["shared_presentation"]["max_initial_surfaces"] = serde_json::json!(17);
        bad.push(excess);
        for body in bad {
            assert!(
                UiBrowserSessionPolicySection
                    .parse(&context(), &body)
                    .is_err()
            );
        }
    }

    #[test]
    fn zero_initial_surfaces_is_valid() {
        let mut body = current_body();
        body["shared_presentation"]["max_initial_surfaces"] = serde_json::json!(0);
        assert!(
            UiBrowserSessionPolicySection
                .parse(&context(), &body)
                .is_ok()
        );
    }
}
