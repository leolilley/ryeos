//! Node-owned resource limits for authenticated UI browser sessions.
//!
//! The UI session store enforces this compiled policy when it admits retained
//! binding attachments, and the seat history stream takes its frame bounds and
//! the seat thread kind from it. Session lifetime, idle timeout, launch-token
//! lifetime and the capability ceiling of every browser session are signed
//! data here as well. The policy has no daemon fallback:
//! registration makes the section a required member of every complete
//! node-policy generation.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "ui_browser_sessions";

/// Most events one replay frame may carry: the cold-replay decoders of the web
/// and terminal clients refuse a longer frame.
pub const MAX_REPLAY_FRAME_EVENTS_CEILING: u32 = 64;

/// Most record bytes one replay frame may carry: half of the smallest client
/// frame buffer (the browser's 16 MiB), which leaves room for the JSON and SSE
/// envelope and string escaping around the records.
pub const MAX_REPLAY_FRAME_BYTES_CEILING: u64 = 8 * 1024 * 1024;

/// Longest browser session lifetime the policy may declare.
pub const SESSION_TTL_SECS_CEILING: u64 = 24 * 60 * 60;

/// Longest launch-token lifetime the policy may declare.
pub const LAUNCH_TOKEN_TTL_SECS_CEILING: u64 = 300;

/// The only schema this section accepts.
pub const CURRENT_SCHEMA: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBrowserSessionPolicy {
    pub schema: u32,
    /// Absolute lifetime of one browser session, from activation.
    pub session_ttl_secs: u64,
    /// A session with no request for this long is expired.
    pub session_idle_secs: u64,
    /// Lifetime of the single-use launch token a mint returns.
    pub launch_token_ttl_secs: u64,
    /// Capability patterns: the most any browser session on this node may
    /// ever hold. A session's grant is the minter's scopes intersected with
    /// this set.
    pub session_scopes: Vec<String>,
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
        if self.schema != CURRENT_SCHEMA {
            bail!(
                "UI browser-session node policy schema {} is not current ({CURRENT_SCHEMA}); \
                 re-issue the ui_browser_sessions section at schema {CURRENT_SCHEMA} with \
                 session_ttl_secs, session_idle_secs, launch_token_ttl_secs and session_scopes",
                self.schema
            );
        }
        for (name, value, ceiling) in [
            (
                "session_ttl_secs",
                self.session_ttl_secs,
                SESSION_TTL_SECS_CEILING,
            ),
            (
                "launch_token_ttl_secs",
                self.launch_token_ttl_secs,
                LAUNCH_TOKEN_TTL_SECS_CEILING,
            ),
        ] {
            if value == 0 {
                bail!("UI browser-session {name} must be non-zero");
            }
            if value > ceiling {
                bail!("UI browser-session {name} exceeds the ceiling of {ceiling} seconds");
            }
        }
        if self.session_idle_secs == 0 {
            bail!("UI browser-session session_idle_secs must be non-zero");
        }
        if self.session_idle_secs > self.session_ttl_secs {
            bail!("UI browser-session session_idle_secs must not exceed session_ttl_secs");
        }
        if self.session_scopes.is_empty() {
            bail!(
                "UI browser-session session_scopes must list at least one capability pattern; \
                 an empty ceiling would leave every browser session with no authority"
            );
        }
        for scope in &self.session_scopes {
            ryeos_runtime::authorizer::validate_scope_pattern(scope).map_err(|reason| {
                anyhow::anyhow!("UI browser-session session_scopes entry is invalid: {reason}")
            })?;
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
        if self.seat.max_replay_frame_events > MAX_REPLAY_FRAME_EVENTS_CEILING {
            bail!(
                "UI browser-session seat.max_replay_frame_events exceeds the client decoder limit of {MAX_REPLAY_FRAME_EVENTS_CEILING}"
            );
        }
        if self.seat.max_replay_frame_bytes > MAX_REPLAY_FRAME_BYTES_CEILING {
            bail!(
                "UI browser-session seat.max_replay_frame_bytes exceeds the client frame budget of {MAX_REPLAY_FRAME_BYTES_CEILING}"
            );
        }
        if self.shared_presentation.max_initial_surfaces
            > self.max_surface_instances_per_presentation
        {
            bail!("UI browser-session initial Surfaces exceed the presentation Surface limit");
        }
        Ok(())
    }

    /// Refuse a seat kind that no installed kind profile declares fenced. The
    /// policy alone cannot know the installed profiles, so the composition
    /// that holds both checks them together before minting a seat.
    pub fn require_fenced_seat_kind(
        &self,
        fenced: &ryeos_state::FencedChainKinds,
    ) -> anyhow::Result<()> {
        if !fenced.contains(&self.seat.kind) {
            bail!(
                "UI browser-session seat.kind `{}` names no kind profile with a retirement fence",
                self.seat.kind
            );
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
            "schema": 5,
            "session_ttl_secs": 28800,
            "session_idle_secs": 3600,
            "launch_token_ttl_secs": 60,
            "session_scopes": ["ryeos.execute.service.ui/*"],
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
            "session_ttl_secs",
            "session_idle_secs",
            "launch_token_ttl_secs",
            "session_scopes",
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
        for predecessor in [1, 2, 3, 4] {
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
    fn session_policy_requires_ttls() {
        for name in [
            "session_ttl_secs",
            "session_idle_secs",
            "launch_token_ttl_secs",
            "session_scopes",
        ] {
            let mut body = current_body();
            body.as_object_mut().unwrap().remove(name);
            assert!(
                UiBrowserSessionPolicySection
                    .parse(&context(), &body)
                    .is_err(),
                "{name} has no code default"
            );
        }
        for name in [
            "session_ttl_secs",
            "session_idle_secs",
            "launch_token_ttl_secs",
        ] {
            let mut zero = current_body();
            zero[name] = serde_json::json!(0);
            assert!(
                UiBrowserSessionPolicySection
                    .parse(&context(), &zero)
                    .is_err()
            );
        }
    }

    #[test]
    fn session_ttl_ceiling_enforced() {
        let mut body = current_body();
        body["session_ttl_secs"] = serde_json::json!(SESSION_TTL_SECS_CEILING);
        assert!(
            UiBrowserSessionPolicySection
                .parse(&context(), &body)
                .is_ok()
        );
        body["session_ttl_secs"] = serde_json::json!(SESSION_TTL_SECS_CEILING + 1);
        let error = UiBrowserSessionPolicySection
            .parse(&context(), &body)
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("session_ttl_secs"),
            "{error:#}"
        );
        let mut body = current_body();
        body["launch_token_ttl_secs"] = serde_json::json!(LAUNCH_TOKEN_TTL_SECS_CEILING + 1);
        assert!(
            UiBrowserSessionPolicySection
                .parse(&context(), &body)
                .is_err()
        );
        let mut body = current_body();
        body["session_idle_secs"] = serde_json::json!(28801);
        assert!(
            UiBrowserSessionPolicySection
                .parse(&context(), &body)
                .is_err(),
            "idle timeout longer than the session lifetime is meaningless"
        );
    }

    #[test]
    fn session_scopes_must_be_non_empty_canonical_patterns() {
        for scopes in [
            serde_json::json!([]),
            serde_json::json!(["ui.read"]),
            serde_json::json!([""]),
        ] {
            let mut body = current_body();
            body["session_scopes"] = scopes;
            assert!(
                UiBrowserSessionPolicySection
                    .parse(&context(), &body)
                    .is_err()
            );
        }
    }

    #[test]
    fn schema_four_is_refused_with_the_cure() {
        let mut body = current_body();
        body["schema"] = serde_json::json!(4);
        let error = UiBrowserSessionPolicySection
            .parse(&context(), &body)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("schema 5"), "{error:#}");
    }

    #[test]
    fn replay_frame_bounds_above_the_client_ceilings_are_refused() {
        for (name, over) in [
            (
                "max_replay_frame_events",
                serde_json::json!(MAX_REPLAY_FRAME_EVENTS_CEILING + 1),
            ),
            (
                "max_replay_frame_bytes",
                serde_json::json!(MAX_REPLAY_FRAME_BYTES_CEILING + 1),
            ),
        ] {
            let mut body = current_body();
            body["seat"][name] = over;
            let error = UiBrowserSessionPolicySection
                .parse(&context(), &body)
                .err()
                .unwrap();
            assert!(format!("{error:#}").contains(name), "{error:#}");
            let mut at_ceiling = current_body();
            at_ceiling["seat"][name] = match name {
                "max_replay_frame_events" => serde_json::json!(MAX_REPLAY_FRAME_EVENTS_CEILING),
                _ => serde_json::json!(MAX_REPLAY_FRAME_BYTES_CEILING),
            };
            assert!(
                UiBrowserSessionPolicySection
                    .parse(&context(), &at_ceiling)
                    .is_ok()
            );
        }
    }

    #[test]
    fn the_seat_kind_must_name_a_fenced_kind_profile() {
        let parsed = UiBrowserSessionPolicySection
            .parse(&context(), &current_body())
            .unwrap();
        let policy = parsed
            .as_any()
            .downcast_ref::<UiBrowserSessionPolicy>()
            .unwrap();
        let fenced = ryeos_state::FencedChainKinds::new(["seat_session".to_owned()]).unwrap();
        policy.require_fenced_seat_kind(&fenced).unwrap();
        for other in [
            ryeos_state::FencedChainKinds::default(),
            ryeos_state::FencedChainKinds::new(["directive".to_owned()]).unwrap(),
        ] {
            let error = policy.require_fenced_seat_kind(&other).unwrap_err();
            assert!(
                format!("{error:#}").contains("retirement fence"),
                "{error:#}"
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
