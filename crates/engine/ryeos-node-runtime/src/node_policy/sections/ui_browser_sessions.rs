//! Node-owned resource limits for authenticated UI browser sessions.
//!
//! The UI session store enforces this compiled policy when it admits retained
//! binding attachments, and the seat history stream takes its frame bounds and
//! the seat thread kind from it. The policy has no daemon fallback:
//! registration makes the section a required member of every complete
//! node-policy generation.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "ui_browser_sessions";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBrowserSessionPolicy {
    pub schema: u32,
    pub max_session_bootstrap_bytes: u64,
    pub max_live_binding_attachments_per_session: u32,
    pub max_presentations_per_session: u32,
    pub max_surface_instances_per_presentation: u32,
    pub shared_presentation: SharedPresentationPolicyLimits,
    pub seat: SeatPolicy,
}

/// The seat thread kind a browser session binds, and the bounds of one
/// provisional replay frame of seat history. The kind names a fenced chain
/// kind; the frame bounds must sit below the smallest client decoder limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeatPolicy {
    pub kind: String,
    pub max_replay_frame_events: u32,
    pub max_replay_frame_bytes: u64,
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
        if self.schema != 4 {
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
        if self.seat.kind.trim().is_empty() || self.seat.kind.trim() != self.seat.kind {
            bail!("UI browser-session seat.kind must be a non-empty trimmed kind name");
        }
        for (name, value) in [
            (
                "seat.max_replay_frame_events",
                u64::from(self.seat.max_replay_frame_events),
            ),
            (
                "seat.max_replay_frame_bytes",
                self.seat.max_replay_frame_bytes,
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
        assert_eq!(policy.seat.kind, "seat_session");
        assert_eq!(policy.seat.max_replay_frame_events, 64);
        assert_eq!(policy.seat.max_replay_frame_bytes, 4194304);
    }

    fn current_body() -> Value {
        serde_json::json!({
            "schema": 4,
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
            },
            "seat": {
                "kind": "seat_session",
                "max_replay_frame_events": 64,
                "max_replay_frame_bytes": 4194304
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
            "seat",
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
        for name in ["max_replay_frame_events", "max_replay_frame_bytes"] {
            let mut zero = original.clone();
            zero["seat"][name] = serde_json::json!(0);
            bad.push(zero);
        }
        for name in ["kind", "max_replay_frame_events", "max_replay_frame_bytes"] {
            let mut missing = original.clone();
            missing["seat"].as_object_mut().unwrap().remove(name);
            bad.push(missing);
        }
        for kind in ["", " seat_session", "seat_session "] {
            let mut blank = original.clone();
            blank["seat"]["kind"] = serde_json::json!(kind);
            bad.push(blank);
        }
        let mut seat_unknown = original.clone();
        seat_unknown["seat"]["fallback"] = serde_json::json!(1);
        bad.push(seat_unknown);
        for predecessor in [1, 2, 3] {
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
