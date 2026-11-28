//! The keyless routes of the public page this node serves, by route id.
//!
//! A node serves a keyless route of the public page only when this signed
//! section names the route's id, with the limits the route is held to: the
//! bytes it may send over a rolling 24 hours and the requests it may work on
//! at once. The limits are the node's, keyed by route id, never reset while
//! the node runs, and never defaulted: a route the section does not name is
//! not served. Every shipped profile names none; only the public node's
//! profile names the public page's routes.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "public_routes";
pub const CURRENT_SCHEMA: u32 = 1;
/// Most requests one public route may work on at once.
pub const MAX_CONCURRENT_CEILING: u32 = 1024;
/// Longest route id the section accepts.
pub const MAX_ROUTE_ID_BYTES: usize = 128;
/// Most routes one node may name.
pub const MAX_ROUTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicRoutesPolicy {
    pub schema: u32,
    /// Route id to its limits. A route not named here is not served.
    pub routes: BTreeMap<String, PublicRouteLimits>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicRouteLimits {
    /// Bytes the route may send over a rolling 24 hours.
    pub daily_bytes: u64,
    /// Requests the route may work on at once.
    pub concurrent_max: u32,
}

fn route_id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ROUTE_ID_BYTES
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'/' | b'-' | b'_')
        })
        && !id.contains("//")
        && !id.ends_with('/')
}

impl PublicRoutesPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != CURRENT_SCHEMA {
            bail!("node public_routes policy schema is not current");
        }
        if self.routes.len() > MAX_ROUTES {
            bail!("node public_routes policy names more than {MAX_ROUTES} routes");
        }
        for (id, limits) in &self.routes {
            if !route_id_ok(id) {
                bail!("node public_routes policy names an invalid route id {id:?}");
            }
            if limits.daily_bytes == 0 {
                bail!("node public_routes policy: route {id} daily_bytes must be positive");
            }
            if limits.concurrent_max == 0 || limits.concurrent_max > MAX_CONCURRENT_CEILING {
                bail!(
                    "node public_routes policy: route {id} concurrent_max must be 1 to {MAX_CONCURRENT_CEILING}"
                );
            }
        }
        Ok(())
    }

    /// The limits for `route_id`, or `None` when the node does not serve it.
    pub fn limits_for(&self, route_id: &str) -> Option<PublicRouteLimits> {
        self.routes.get(route_id).copied()
    }

    /// The section every shipped profile carries: no public route.
    pub fn as_shipped() -> Self {
        Self {
            schema: CURRENT_SCHEMA,
            routes: BTreeMap::new(),
        }
    }
}

impl TypedNodePolicy for PublicRoutesPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct PublicRoutesPolicySection;

impl NodePolicySection for PublicRoutesPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: PublicRoutesPolicy =
            serde_json::from_value(body.clone()).context("parse node public_routes policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(id: &str, daily_bytes: u64, concurrent_max: u32) -> PublicRoutesPolicy {
        PublicRoutesPolicy {
            schema: CURRENT_SCHEMA,
            routes: [(
                id.to_owned(),
                PublicRouteLimits {
                    daily_bytes,
                    concurrent_max,
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn the_shipped_policy_names_no_route() {
        let shipped = PublicRoutesPolicy::as_shipped();
        shipped.validate().unwrap();
        assert!(shipped.routes.is_empty());
        assert_eq!(shipped.limits_for("public/front"), None);
    }

    #[test]
    fn a_named_route_has_its_limits_and_nothing_else_is_served() {
        let policy = one("public/front", 1 << 20, 8);
        policy.validate().unwrap();
        assert_eq!(
            policy.limits_for("public/front"),
            Some(PublicRouteLimits {
                daily_bytes: 1 << 20,
                concurrent_max: 8
            })
        );
        assert_eq!(policy.limits_for("public/media"), None);
    }

    #[test]
    fn bad_policies_are_refused() {
        let mut wrong_schema = PublicRoutesPolicy::as_shipped();
        wrong_schema.schema = 2;
        assert!(wrong_schema.validate().is_err());
        assert!(one("public/front", 0, 8).validate().is_err());
        assert!(one("public/front", 1, 0).validate().is_err());
        assert!(
            one("public/front", 1, MAX_CONCURRENT_CEILING + 1)
                .validate()
                .is_err()
        );
        for id in [
            "",
            "Public/front",
            "public//front",
            "public/front/",
            "1public",
            "public front",
            "public:front",
        ] {
            assert!(
                one(id, 1, 1).validate().is_err(),
                "route id {id:?} accepted"
            );
        }
        assert!(
            one(&"a".repeat(MAX_ROUTE_ID_BYTES + 1), 1, 1)
                .validate()
                .is_err()
        );
        let too_many = PublicRoutesPolicy {
            schema: CURRENT_SCHEMA,
            routes: (0..=MAX_ROUTES)
                .map(|n| {
                    (
                        format!("public/r{n}"),
                        PublicRouteLimits {
                            daily_bytes: 1,
                            concurrent_max: 1,
                        },
                    )
                })
                .collect(),
        };
        assert!(too_many.validate().is_err());
    }

    #[test]
    fn unknown_fields_are_refused() {
        let body = serde_json::json!({ "schema": 1, "routes": {}, "default": {} });
        assert!(serde_json::from_value::<PublicRoutesPolicy>(body).is_err());
        let body = serde_json::json!({
            "schema": 1,
            "routes": { "public/front": { "daily_bytes": 1, "concurrent_max": 1, "extra": 1 } }
        });
        assert!(serde_json::from_value::<PublicRoutesPolicy>(body).is_err());
    }
}
