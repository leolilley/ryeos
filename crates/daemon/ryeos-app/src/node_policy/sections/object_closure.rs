//! Node-owned admission limits for remote CAS closure transfer.
//!
//! Object schemas and the wire protocol retain their absolute safety bounds.
//! This policy selects how much of that capacity this node is willing to
//! serve or receive in one operation. A workflow or caller may narrow these
//! limits but can never widen them.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "object_closure";
const RESPONSE_ENVELOPE_BYTES: u64 = 4 * 1024;
const RESPONSE_ENTRY_OVERHEAD_BYTES: u64 = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeObjectClosurePolicy {
    pub schema: u32,
    pub max_roots: usize,
    pub max_objects: usize,
    pub max_blobs: usize,
    pub max_object_bytes: u64,
    pub max_total_object_bytes: u64,
    pub max_blob_bytes: u64,
    pub max_total_blob_bytes: u64,
    pub max_response_bytes: u64,
    pub max_links_per_object: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestedObjectTransferLimits {
    pub max_objects: Option<usize>,
    pub max_blobs: Option<usize>,
    pub max_object_bytes: Option<u64>,
    pub max_total_object_bytes: Option<u64>,
    pub max_blob_bytes: Option<u64>,
    pub max_total_blob_bytes: Option<u64>,
    pub max_response_bytes: Option<u64>,
    pub max_links_per_object: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmittedObjectTransferLimits {
    pub max_roots: usize,
    pub max_objects: usize,
    pub max_blobs: usize,
    pub max_object_bytes: u64,
    pub max_total_object_bytes: u64,
    pub max_blob_bytes: u64,
    pub max_total_blob_bytes: u64,
    pub max_response_bytes: u64,
    pub max_links_per_object: usize,
}

impl NodeObjectClosurePolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("node object-closure policy schema is not current");
        }
        validate_usize_limit(
            "max_roots",
            self.max_roots,
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_ROOTS,
        )?;
        validate_usize_limit(
            "max_objects",
            self.max_objects,
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_OBJECTS,
        )?;
        validate_usize_limit(
            "max_blobs",
            self.max_blobs,
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_BLOBS,
        )?;
        validate_u64_limit(
            "max_object_bytes",
            self.max_object_bytes,
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_OBJECT_BYTES,
        )?;
        validate_u64_limit(
            "max_total_object_bytes",
            self.max_total_object_bytes,
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_TOTAL_OBJECT_BYTES,
        )?;
        validate_u64_limit(
            "max_blob_bytes",
            self.max_blob_bytes,
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_BLOB_BYTES,
        )?;
        validate_u64_limit(
            "max_total_blob_bytes",
            self.max_total_blob_bytes,
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_TOTAL_BLOB_BYTES,
        )?;
        validate_u64_limit(
            "max_response_bytes",
            self.max_response_bytes,
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_RESPONSE_BYTES,
        )?;
        validate_usize_limit(
            "max_links_per_object",
            self.max_links_per_object,
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_LINKS_PER_OBJECT,
        )?;
        if self.max_object_bytes > self.max_total_object_bytes {
            bail!("node object-closure max_object_bytes exceeds its aggregate object budget");
        }
        if self.max_blob_bytes > self.max_total_blob_bytes {
            bail!("node object-closure max_blob_bytes exceeds its aggregate blob budget");
        }
        let minimum_response = self.minimum_response_bytes()?;
        if self.max_response_bytes < minimum_response {
            bail!(
                "node object-closure max_response_bytes is too small for its admitted payload: {} < {}",
                self.max_response_bytes,
                minimum_response
            );
        }
        Ok(())
    }

    pub fn max_staged_payload_bytes(&self) -> anyhow::Result<u64> {
        self.max_total_object_bytes
            .checked_add(self.max_total_blob_bytes)
            .context("node object-closure staged payload budget overflow")
    }

    pub fn admit(
        &self,
        requested: RequestedObjectTransferLimits,
    ) -> anyhow::Result<AdmittedObjectTransferLimits> {
        self.validate()?;
        Ok(AdmittedObjectTransferLimits {
            max_roots: self.max_roots,
            max_objects: admit_usize("max_objects", requested.max_objects, self.max_objects)?,
            max_blobs: admit_usize("max_blobs", requested.max_blobs, self.max_blobs)?,
            max_object_bytes: admit_u64(
                "max_object_bytes",
                requested.max_object_bytes,
                self.max_object_bytes,
            )?,
            max_total_object_bytes: admit_u64(
                "max_total_object_bytes",
                requested.max_total_object_bytes,
                self.max_total_object_bytes,
            )?,
            max_blob_bytes: admit_u64(
                "max_blob_bytes",
                requested.max_blob_bytes,
                self.max_blob_bytes,
            )?,
            max_total_blob_bytes: admit_u64(
                "max_total_blob_bytes",
                requested.max_total_blob_bytes,
                self.max_total_blob_bytes,
            )?,
            max_response_bytes: admit_u64(
                "max_response_bytes",
                requested.max_response_bytes,
                self.max_response_bytes,
            )?,
            max_links_per_object: admit_usize(
                "max_links_per_object",
                requested.max_links_per_object,
                self.max_links_per_object,
            )?,
        })
    }

    /// Intersect a peer's requested response ceilings with this serving
    /// node's policy. Peer values are upper bounds, not a demand that the
    /// server widen its own authority. Local receive/workflow admission uses
    /// [`Self::admit`] instead so local callers still fail on attempted
    /// widening rather than silently changing their requested contract.
    pub fn intersect_for_serving(
        &self,
        requested: RequestedObjectTransferLimits,
    ) -> anyhow::Result<AdmittedObjectTransferLimits> {
        self.validate()?;
        Ok(AdmittedObjectTransferLimits {
            max_roots: self.max_roots,
            max_objects: intersect_usize("max_objects", requested.max_objects, self.max_objects)?,
            max_blobs: intersect_usize("max_blobs", requested.max_blobs, self.max_blobs)?,
            max_object_bytes: intersect_u64(
                "max_object_bytes",
                requested.max_object_bytes,
                self.max_object_bytes,
            )?,
            max_total_object_bytes: intersect_u64(
                "max_total_object_bytes",
                requested.max_total_object_bytes,
                self.max_total_object_bytes,
            )?,
            max_blob_bytes: intersect_u64(
                "max_blob_bytes",
                requested.max_blob_bytes,
                self.max_blob_bytes,
            )?,
            max_total_blob_bytes: intersect_u64(
                "max_total_blob_bytes",
                requested.max_total_blob_bytes,
                self.max_total_blob_bytes,
            )?,
            max_response_bytes: intersect_u64(
                "max_response_bytes",
                requested.max_response_bytes,
                self.max_response_bytes,
            )?,
            max_links_per_object: intersect_usize(
                "max_links_per_object",
                requested.max_links_per_object,
                self.max_links_per_object,
            )?,
        })
    }

    fn minimum_response_bytes(&self) -> anyhow::Result<u64> {
        let encoded_blob_bytes = self
            .max_total_blob_bytes
            .checked_add(2)
            .context("node object-closure base64 budget overflow")?
            / 3
            * 4;
        let entry_count = u64::try_from(self.max_objects)
            .context("node object-closure max_objects does not fit u64")?
            .checked_add(
                u64::try_from(self.max_blobs)
                    .context("node object-closure max_blobs does not fit u64")?,
            )
            .context("node object-closure entry-count overflow")?;
        RESPONSE_ENVELOPE_BYTES
            .checked_add(self.max_total_object_bytes)
            .and_then(|value| value.checked_add(encoded_blob_bytes))
            .and_then(|value| {
                entry_count
                    .checked_mul(RESPONSE_ENTRY_OVERHEAD_BYTES)
                    .and_then(|overhead| value.checked_add(overhead))
            })
            .context("node object-closure response budget overflow")
    }
}

fn validate_usize_limit(label: &str, value: usize, maximum: usize) -> anyhow::Result<()> {
    if value == 0 || value > maximum {
        bail!("node object-closure {label} must be between 1 and {maximum}");
    }
    Ok(())
}

fn validate_u64_limit(label: &str, value: u64, maximum: u64) -> anyhow::Result<()> {
    if value == 0 || value > maximum {
        bail!("node object-closure {label} must be between 1 and {maximum}");
    }
    Ok(())
}

fn admit_usize(label: &str, requested: Option<usize>, policy: usize) -> anyhow::Result<usize> {
    match requested {
        Some(value) if value == 0 || value > policy => {
            bail!("object-closure {label} exceeds node policy: {value} > {policy}")
        }
        Some(value) => Ok(value),
        None => Ok(policy),
    }
}

fn admit_u64(label: &str, requested: Option<u64>, policy: u64) -> anyhow::Result<u64> {
    match requested {
        Some(value) if value == 0 || value > policy => {
            bail!("object-closure {label} exceeds node policy: {value} > {policy}")
        }
        Some(value) => Ok(value),
        None => Ok(policy),
    }
}

fn intersect_usize(label: &str, requested: Option<usize>, policy: usize) -> anyhow::Result<usize> {
    match requested {
        Some(0) => bail!("object-closure {label} must be positive"),
        Some(value) => Ok(value.min(policy)),
        None => Ok(policy),
    }
}

fn intersect_u64(label: &str, requested: Option<u64>, policy: u64) -> anyhow::Result<u64> {
    match requested {
        Some(0) => bail!("object-closure {label} must be positive"),
        Some(value) => Ok(value.min(policy)),
        None => Ok(policy),
    }
}

impl TypedNodePolicy for NodeObjectClosurePolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct NodeObjectClosurePolicySection;

impl NodePolicySection for NodeObjectClosurePolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: NodeObjectClosurePolicy =
            serde_json::from_value(body.clone()).context("parse node object-closure policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_policy() -> NodeObjectClosurePolicy {
        NodeObjectClosurePolicy {
            schema: 1,
            max_roots: 256,
            max_objects: 32_768,
            max_blobs: 32_768,
            max_object_bytes: 32 * 1024 * 1024,
            max_total_object_bytes: 64 * 1024 * 1024,
            max_blob_bytes: 128 * 1024 * 1024,
            max_total_blob_bytes: 128 * 1024 * 1024,
            max_response_bytes: 256 * 1024 * 1024,
            max_links_per_object: 100_000,
        }
    }

    #[test]
    fn current_policy_is_coherent_and_covers_qualification_snapshot() {
        let policy = valid_policy();
        policy.validate().unwrap();
        assert!(policy.max_total_blob_bytes >= 50_359_567);
        assert_eq!(
            policy.max_staged_payload_bytes().unwrap(),
            192 * 1024 * 1024
        );
    }

    #[test]
    fn policy_rejects_zero_absolute_and_incoherent_limits() {
        let mut policy = valid_policy();
        policy.max_objects = 0;
        assert!(policy.validate().is_err());

        let mut policy = valid_policy();
        policy.max_roots = ryeos_state::object_closure::REMOTE_CLOSURE_MAX_ROOTS + 1;
        assert!(policy.validate().is_err());

        let mut policy = valid_policy();
        policy.max_blob_bytes = policy.max_total_blob_bytes + 1;
        assert!(policy.validate().is_err());

        let mut policy = valid_policy();
        policy.max_response_bytes = 128 * 1024 * 1024;
        assert!(policy.validate().is_err());

        let mut policy = valid_policy();
        policy.max_links_per_object =
            ryeos_state::object_closure::REMOTE_CLOSURE_MAX_LINKS_PER_OBJECT + 1;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn callers_may_narrow_but_never_widen_node_policy() {
        let policy = valid_policy();
        let narrowed = policy
            .admit(RequestedObjectTransferLimits {
                max_objects: Some(16),
                max_total_blob_bytes: Some(50_359_567),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(narrowed.max_objects, 16);
        assert_eq!(narrowed.max_total_blob_bytes, 50_359_567);
        assert_eq!(narrowed.max_response_bytes, policy.max_response_bytes);

        let error = policy
            .admit(RequestedObjectTransferLimits {
                max_total_blob_bytes: Some(policy.max_total_blob_bytes + 1),
                ..Default::default()
            })
            .unwrap_err();
        assert!(error.to_string().contains("exceeds node policy"));
    }

    #[test]
    fn serving_intersects_a_wider_peer_with_local_node_policy() {
        let policy = valid_policy();
        let admitted = policy
            .intersect_for_serving(RequestedObjectTransferLimits {
                max_objects: Some(policy.max_objects + 1),
                max_total_object_bytes: Some(policy.max_total_object_bytes + 1),
                max_response_bytes: Some(policy.max_response_bytes + 1),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(admitted.max_objects, policy.max_objects);
        assert_eq!(
            admitted.max_total_object_bytes,
            policy.max_total_object_bytes
        );
        assert_eq!(admitted.max_response_bytes, policy.max_response_bytes);
        assert!(
            policy
                .intersect_for_serving(RequestedObjectTransferLimits {
                    max_objects: Some(0),
                    ..Default::default()
                })
                .is_err()
        );
    }
}
