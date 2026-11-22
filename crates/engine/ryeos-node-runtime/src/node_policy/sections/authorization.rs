//! Node-owned bounds on how long an authorized-key grant may live.
//!
//! Every node-signed grant carries an expiry. This section is where the
//! operator states, in the signed policy generation, the longest lifetime the
//! node will issue for each principal class and how early the doctor should
//! warn that a grant is about to lapse. A writer that is asked for a longer
//! lifetime refuses with the class maximum; there is no daemon fallback, so
//! registration makes the section a required member of every generation.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::identity::{AuthorizedKeyPrincipalClass, GrantExpiry, GrantLifetimeError};
use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "authorization";

/// No policy may grant a lifetime beyond ten years: a longer bound is an
/// operator typo, not a lifetime.
const MAX_POLICY_LIFETIME_SECS: u64 = 10 * 365 * 24 * 60 * 60;

/// The longest lifetime, in seconds, of a grant of each principal class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantLifetimes {
    pub local_client: u64,
    pub remote_node: u64,
    pub remote_principal: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeAuthorizationPolicy {
    pub schema: u32,
    /// Longest lifetime a grant of each class may be issued or renewed for.
    /// A request that names no lifetime receives this maximum.
    pub max_lifetime_secs: GrantLifetimes,
    /// How long before expiry the doctor starts reporting a grant as expiring.
    pub warn_before_secs: u64,
}

impl NodeAuthorizationPolicy {
    /// A policy with the shipped standard lifetimes, for exact test snapshots.
    pub fn standard() -> Self {
        Self {
            schema: 1,
            max_lifetime_secs: GrantLifetimes {
                local_client: 365 * 24 * 60 * 60,
                remote_node: 90 * 24 * 60 * 60,
                remote_principal: 30 * 24 * 60 * 60,
            },
            warn_before_secs: 14 * 24 * 60 * 60,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("node authorization policy schema is not current");
        }
        for (name, value) in [
            ("local_client", self.max_lifetime_secs.local_client),
            ("remote_node", self.max_lifetime_secs.remote_node),
            ("remote_principal", self.max_lifetime_secs.remote_principal),
        ] {
            if value == 0 || value > MAX_POLICY_LIFETIME_SECS {
                bail!(
                    "node authorization policy max_lifetime_secs.{name} must be between 1 and \
                     {MAX_POLICY_LIFETIME_SECS} seconds"
                );
            }
        }
        if self.warn_before_secs == 0 {
            bail!("node authorization policy warn_before_secs must be non-zero");
        }
        Ok(())
    }

    /// Read and verify the `authorization` member of a stopped node's signed
    /// policy generation.
    pub fn load_offline(
        app_root: &std::path::Path,
        node: &crate::identity::NodeIdentity,
    ) -> anyhow::Result<Self> {
        let body = crate::node_policy::generation::load_signed_policy_member(
            app_root,
            node,
            SECTION_NAME,
        )?;
        let record: Self =
            serde_json::from_value(body).context("parse node authorization policy")?;
        record.validate()?;
        Ok(record)
    }

    /// The longest lifetime this policy allows a grant of `class`.
    pub fn max_lifetime_secs(&self, class: AuthorizedKeyPrincipalClass) -> u64 {
        match class {
            AuthorizedKeyPrincipalClass::LocalClient => self.max_lifetime_secs.local_client,
            AuthorizedKeyPrincipalClass::RemoteNode => self.max_lifetime_secs.remote_node,
            AuthorizedKeyPrincipalClass::RemotePrincipal => self.max_lifetime_secs.remote_principal,
        }
    }

    /// The expiry of a grant of `class` issued at `now` (Unix seconds) for
    /// `requested_secs`, or for the class maximum when none is requested.
    /// Refuses a lifetime above the class maximum or of zero seconds. The
    /// result is never [`GrantExpiry::Never`]: only the node's configured
    /// operator grant is issued without an end, and not through policy.
    pub fn expiry_for(
        &self,
        class: AuthorizedKeyPrincipalClass,
        requested_secs: Option<u64>,
        now: u64,
    ) -> Result<GrantExpiry, GrantLifetimeError> {
        let maximum_secs = self.max_lifetime_secs(class);
        let lifetime_secs = requested_secs.unwrap_or(maximum_secs);
        if lifetime_secs == 0 {
            return Err(GrantLifetimeError::Zero);
        }
        if lifetime_secs > maximum_secs {
            return Err(GrantLifetimeError::ExceedsPolicy {
                class,
                requested_secs: lifetime_secs,
                maximum_secs,
            });
        }
        Ok(GrantExpiry::At(now.saturating_add(lifetime_secs)))
    }
}

impl TypedNodePolicy for NodeAuthorizationPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct NodeAuthorizationPolicySection;

impl NodePolicySection for NodeAuthorizationPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: NodeAuthorizationPolicy =
            serde_json::from_value(body.clone()).context("parse node authorization policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: Value) -> anyhow::Result<NodeAuthorizationPolicy> {
        let record: NodeAuthorizationPolicy = serde_json::from_value(body)?;
        record.validate()?;
        Ok(record)
    }

    fn body() -> Value {
        serde_json::json!({
            "schema": 1,
            "max_lifetime_secs": {
                "local_client": 31536000,
                "remote_node": 7776000,
                "remote_principal": 2592000,
            },
            "warn_before_secs": 1209600,
        })
    }

    #[test]
    fn section_shape_is_the_standard_policy() {
        assert_eq!(parse(body()).unwrap(), NodeAuthorizationPolicy::standard());
    }

    #[test]
    fn malformed_shapes_are_refused() {
        let mut wrong_schema = body();
        wrong_schema["schema"] = serde_json::json!(2);
        assert!(parse(wrong_schema).is_err());
        let mut zero = body();
        zero["max_lifetime_secs"]["remote_node"] = serde_json::json!(0);
        assert!(parse(zero).is_err());
        let mut huge = body();
        huge["max_lifetime_secs"]["local_client"] = serde_json::json!(MAX_POLICY_LIFETIME_SECS + 1);
        assert!(parse(huge).is_err());
        let mut missing = body();
        missing["max_lifetime_secs"]
            .as_object_mut()
            .unwrap()
            .remove("remote_principal");
        assert!(parse(missing).is_err());
        let mut extra = body();
        extra["extra"] = serde_json::json!(true);
        assert!(parse(extra).is_err());
        let mut no_warning = body();
        no_warning["warn_before_secs"] = serde_json::json!(0);
        assert!(parse(no_warning).is_err());
    }

    #[test]
    fn lifetime_over_maximum_is_refused_naming_the_maximum() {
        let policy = NodeAuthorizationPolicy::standard();
        let thirty_days = 30 * 24 * 60 * 60;
        let error = policy
            .expiry_for(
                AuthorizedKeyPrincipalClass::RemotePrincipal,
                Some(thirty_days + 1),
                1_000,
            )
            .unwrap_err();
        assert_eq!(
            error,
            GrantLifetimeError::ExceedsPolicy {
                class: AuthorizedKeyPrincipalClass::RemotePrincipal,
                requested_secs: thirty_days + 1,
                maximum_secs: thirty_days,
            }
        );
        assert_eq!(error.code(), "grant_lifetime_exceeds_policy");
        assert!(error.to_string().contains(&thirty_days.to_string()));
    }

    #[test]
    fn an_unrequested_lifetime_is_the_class_maximum_and_never_is_not_reachable() {
        let policy = NodeAuthorizationPolicy::standard();
        for class in [
            AuthorizedKeyPrincipalClass::LocalClient,
            AuthorizedKeyPrincipalClass::RemoteNode,
            AuthorizedKeyPrincipalClass::RemotePrincipal,
        ] {
            assert_eq!(
                policy.expiry_for(class, None, 1_000).unwrap(),
                GrantExpiry::At(1_000 + policy.max_lifetime_secs(class))
            );
            assert_eq!(
                policy.expiry_for(class, Some(60), 1_000).unwrap(),
                GrantExpiry::At(1_060)
            );
            assert_eq!(
                policy.expiry_for(class, Some(0), 1_000).unwrap_err(),
                GrantLifetimeError::Zero
            );
        }
    }
}
