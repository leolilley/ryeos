//! Node-owned limits on the attachments a thread's events may name.
//!
//! An attachment is opaque bytes stored beside a thread event and referenced
//! from it. The node decides how large one may be, how many one turn and one
//! run may add, and which media types it accepts. The section has no daemon
//! fallback: registration makes it a required member of every complete
//! node-policy generation. The ceilings below keep a node from setting a limit
//! its own object-closure policy could not transfer.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use ryeos_state::objects::thread_attachment::{MAX_THREAD_ATTACHMENT_BYTES, validate_media_type};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "thread_attachments";

/// Most attachments one turn may add, whatever the node configures.
pub const MAX_ATTACHMENTS_PER_TURN_CEILING: u32 = 64;
/// Most attachments one run may add, whatever the node configures.
pub const MAX_ATTACHMENTS_PER_RUN_CEILING: u32 = 65_536;
/// Most media types a node may accept.
pub const MAX_ACCEPTED_MEDIA_TYPES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadAttachmentsPolicy {
    pub schema: u32,
    /// Most bytes one attachment may hold.
    pub max_bytes_per_attachment: u64,
    /// Most attachments one turn may add.
    pub max_attachments_per_turn: u32,
    /// Most attachments one run may add.
    pub max_attachments_per_run: u32,
    /// Media types an attachment may carry, as exact `type/subtype` tokens.
    pub accepted_media_types: Vec<String>,
}

impl ThreadAttachmentsPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("node thread_attachments policy schema is not current");
        }
        if self.max_bytes_per_attachment == 0
            || self.max_bytes_per_attachment > MAX_THREAD_ATTACHMENT_BYTES
        {
            bail!(
                "node thread_attachments max_bytes_per_attachment must be 1..={MAX_THREAD_ATTACHMENT_BYTES}"
            );
        }
        if self.max_attachments_per_turn == 0
            || self.max_attachments_per_turn > MAX_ATTACHMENTS_PER_TURN_CEILING
        {
            bail!(
                "node thread_attachments max_attachments_per_turn must be 1..={MAX_ATTACHMENTS_PER_TURN_CEILING}"
            );
        }
        if self.max_attachments_per_run == 0
            || self.max_attachments_per_run > MAX_ATTACHMENTS_PER_RUN_CEILING
        {
            bail!(
                "node thread_attachments max_attachments_per_run must be 1..={MAX_ATTACHMENTS_PER_RUN_CEILING}"
            );
        }
        if self.max_attachments_per_turn > self.max_attachments_per_run {
            bail!(
                "node thread_attachments max_attachments_per_turn exceeds max_attachments_per_run"
            );
        }
        if self.accepted_media_types.is_empty()
            || self.accepted_media_types.len() > MAX_ACCEPTED_MEDIA_TYPES
        {
            bail!(
                "node thread_attachments accepted_media_types must hold 1..={MAX_ACCEPTED_MEDIA_TYPES} entries"
            );
        }
        let mut seen = BTreeSet::new();
        for media_type in &self.accepted_media_types {
            validate_media_type(media_type)
                .with_context(|| format!("node thread_attachments accepts `{media_type}`"))?;
            if !seen.insert(media_type.as_str()) {
                bail!("node thread_attachments accepts `{media_type}` twice");
            }
        }
        Ok(())
    }

    /// Whether the node accepts this media type.
    pub fn accepts(&self, media_type: &str) -> bool {
        self.accepted_media_types
            .iter()
            .any(|accepted| accepted == media_type)
    }

    /// The limits every shipped init profile carries. Builds exact test
    /// snapshots only; a node's policy always comes from its signed
    /// generation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn as_shipped() -> Self {
        Self {
            schema: 1,
            max_bytes_per_attachment: 4_194_304,
            max_attachments_per_turn: 16,
            max_attachments_per_run: 256,
            accepted_media_types: [
                "application/json",
                "image/gif",
                "image/jpeg",
                "image/png",
                "image/webp",
                "text/plain",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        }
    }
}

impl TypedNodePolicy for ThreadAttachmentsPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct ThreadAttachmentsPolicySection;

impl NodePolicySection for ThreadAttachmentsPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: ThreadAttachmentsPolicy =
            serde_json::from_value(body.clone()).context("parse node thread_attachments policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context() -> NodePolicyContext {
        NodePolicyContext {
            section: SECTION_NAME.to_owned(),
            source_file: "/node/policies/thread_attachments.yaml".into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    fn current_body() -> Value {
        json!({
            "schema": 1,
            "max_bytes_per_attachment": 4194304,
            "max_attachments_per_turn": 16,
            "max_attachments_per_run": 256,
            "accepted_media_types": [
                "application/json",
                "image/gif",
                "image/jpeg",
                "image/png",
                "image/webp"
            ]
        })
    }

    #[test]
    fn the_shipped_body_parses_and_matches_the_shipped_record() {
        let parsed = ThreadAttachmentsPolicySection
            .parse(&context(), &current_body())
            .unwrap();
        let policy = parsed
            .as_any()
            .downcast_ref::<ThreadAttachmentsPolicy>()
            .unwrap();
        assert_eq!(policy, &ThreadAttachmentsPolicy::as_shipped());
        assert!(policy.accepts("image/png"));
        assert!(!policy.accepts("image/bmp"));
    }

    #[test]
    fn absent_stale_unknown_and_out_of_range_bodies_are_refused() {
        let original = current_body();
        let mut bad = Vec::new();
        for name in [
            "schema",
            "max_bytes_per_attachment",
            "max_attachments_per_turn",
            "max_attachments_per_run",
            "accepted_media_types",
        ] {
            let mut missing = original.clone();
            missing.as_object_mut().unwrap().remove(name);
            bad.push(missing);
        }
        for (name, value) in [
            ("schema", json!(2)),
            ("max_bytes_per_attachment", json!(0)),
            (
                "max_bytes_per_attachment",
                json!(MAX_THREAD_ATTACHMENT_BYTES + 1),
            ),
            ("max_attachments_per_turn", json!(0)),
            ("max_attachments_per_turn", json!(65)),
            ("max_attachments_per_run", json!(0)),
            ("max_attachments_per_run", json!(65_537)),
            ("max_attachments_per_run", json!(8)),
            ("accepted_media_types", json!([])),
            ("accepted_media_types", json!(["image/png", "image/png"])),
            ("accepted_media_types", json!(["Image/PNG"])),
        ] {
            let mut body = original.clone();
            body[name] = value;
            bad.push(body);
        }
        let mut unknown = original;
        unknown["fallback"] = json!(1);
        bad.push(unknown);
        for body in bad {
            assert!(
                ThreadAttachmentsPolicySection
                    .parse(&context(), &body)
                    .is_err(),
                "{body}"
            );
        }
    }
}
