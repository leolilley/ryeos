//! What this node admits from a bundle that arrived through a publication pull.
//!
//! `admit_activations` names which kinds of item the lane admits by default;
//! only `invoked` is valid (items that act only when a caller invokes them
//! under that caller's own grants), so no default-wide lift of node-loaded
//! items exists. `named_bundles` is the operator's way to admit more for one
//! bundle by name: the exact node-loaded `kinds` it may carry, and the
//! manifest `facets` it may declare. What no entry can name stays refused:
//! provided kinds, the operator vault, state homes, route capabilities,
//! isolation backends, external providers and adapters, executors and native
//! payloads. The shipped profiles carry `admit_activations: [invoked]` and no
//! named bundle; a missing section is an incomplete policy generation, and no
//! default lives in code.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use ryeos_bundle::lane::{Facet, LaneGrants, NEVER_LIFTABLE};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "publication_lane";

/// The one activation the default may admit.
const INVOKED: &str = "invoked";

/// Most named bundles, and most kinds or facets per bundle, node data may carry.
const MAX_NAMED_BUNDLES: usize = 256;
const MAX_ENTRIES_PER_BUNDLE: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationLanePolicy {
    pub schema: u32,
    pub admit_activations: Vec<String>,
    /// The manifest facets an operator may lift for a named bundle. Policy
    /// data, not code; it can only hold facets the manifest format knows and
    /// can never hold one of the never-liftable floor.
    pub liftable_facets: Vec<String>,
    pub named_bundles: BTreeMap<String, NamedBundleLane>,
}

/// What one named bundle may carry beyond the default.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedBundleLane {
    /// Exact node-loaded kinds, as signed data. A kind this node does not have
    /// is accepted here and refused at admission, because a bundle's kinds may
    /// not be installed yet.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Manifest facets, by name; only the liftable ones are valid.
    #[serde(default)]
    pub facets: Vec<String>,
}

fn is_name(value: &str, extra: &[char]) -> bool {
    (1..=128).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || extra.contains(&c))
}

fn sorted_unique(values: &[String]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

impl PublicationLanePolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("node publication lane policy schema is not current");
        }
        if self.admit_activations.iter().any(|value| value != INVOKED) {
            bail!(
                "node publication lane policy admit_activations accepts only `{INVOKED}`; \
                 node-loaded items are lifted per kind in named_bundles"
            );
        }
        if self.admit_activations.len() > 1 {
            bail!("node publication lane policy admit_activations lists `{INVOKED}` once");
        }
        if !sorted_unique(&self.liftable_facets) {
            bail!("node publication lane policy liftable_facets must be sorted and unique");
        }
        for facet in &self.liftable_facets {
            if NEVER_LIFTABLE.contains(&facet.as_str()) {
                bail!(
                    "node publication lane policy liftable_facets lists `{facet}`, which no \
                     policy can lift"
                );
            }
            if Facet::parse(facet).is_none() {
                bail!(
                    "node publication lane policy liftable_facets lists `{facet}`, which the \
                     bundle manifest format does not know"
                );
            }
        }
        if self.named_bundles.len() > MAX_NAMED_BUNDLES {
            bail!("node publication lane policy names more than {MAX_NAMED_BUNDLES} bundles");
        }
        for (bundle, entry) in &self.named_bundles {
            if !is_name(bundle, &['-', '_', '.']) {
                bail!("node publication lane policy bundle name `{bundle}` is not a plain name");
            }
            if entry.kinds.len() > MAX_ENTRIES_PER_BUNDLE
                || entry.facets.len() > MAX_ENTRIES_PER_BUNDLE
            {
                bail!("node publication lane policy entry `{bundle}` is too long");
            }
            if !sorted_unique(&entry.kinds) || !sorted_unique(&entry.facets) {
                bail!(
                    "node publication lane policy entry `{bundle}` must list kinds and facets \
                     sorted and unique"
                );
            }
            for kind in &entry.kinds {
                if !is_name(kind, &['_']) {
                    bail!("node publication lane policy kind `{kind}` is not a plain kind name");
                }
            }
            for facet in &entry.facets {
                if !self.liftable_facets.contains(facet) {
                    bail!(
                        "node publication lane policy facet `{facet}` for `{bundle}` is not in \
                         liftable_facets"
                    );
                }
            }
        }
        Ok(())
    }

    /// What this node admits for `bundle`: the default, plus its named entry.
    pub fn grants_for(&self, bundle: &str) -> LaneGrants {
        let entry = self.named_bundles.get(bundle);
        LaneGrants {
            invoked: self.admit_activations.iter().any(|value| value == INVOKED),
            kinds: entry.map(|entry| entry.kinds.clone()).unwrap_or_default(),
            facets: entry
                .map(|entry| {
                    entry
                        .facets
                        .iter()
                        .filter_map(|facet| Facet::parse(facet))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// The policy every shipped init profile carries. Builds exact test
    /// snapshots only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn as_shipped() -> Self {
        Self {
            schema: 1,
            admit_activations: vec![INVOKED.to_string()],
            liftable_facets: [
                "bundle_events",
                "item_authoring",
                "owns_namespaces",
                "project_snapshots",
                "runtime_vault",
            ]
            .map(String::from)
            .to_vec(),
            named_bundles: BTreeMap::new(),
        }
    }
}

impl TypedNodePolicy for PublicationLanePolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct PublicationLanePolicySection;

impl NodePolicySection for PublicationLanePolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: PublicationLanePolicy =
            serde_json::from_value(body.clone()).context("parse node publication lane policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(kinds: &[&str], facets: &[&str]) -> NamedBundleLane {
        NamedBundleLane {
            kinds: kinds.iter().map(|kind| kind.to_string()).collect(),
            facets: facets.iter().map(|facet| facet.to_string()).collect(),
        }
    }

    fn with(entry: NamedBundleLane) -> PublicationLanePolicy {
        let mut policy = PublicationLanePolicy::as_shipped();
        policy.named_bundles.insert("demo".to_string(), entry);
        policy
    }

    #[test]
    fn the_shipped_policy_validates_and_admits_invoked_items_only() {
        let shipped = PublicationLanePolicy::as_shipped();
        shipped.validate().unwrap();
        let grants = shipped.grants_for("anything");
        assert!(grants.invoked && grants.kinds.is_empty() && grants.facets.is_empty());
    }

    #[test]
    fn node_loaded_cannot_be_admitted_by_default() {
        let mut policy = PublicationLanePolicy::as_shipped();
        policy.admit_activations = vec!["node_loaded".to_string()];
        assert!(policy.validate().is_err());
        policy.admit_activations = vec!["invoked".to_string(), "invoked".to_string()];
        assert!(policy.validate().is_err());
        policy.admit_activations = Vec::new();
        policy.validate().unwrap();
        assert!(!policy.grants_for("demo").invoked);
    }

    #[test]
    fn the_liftable_list_can_never_hold_a_floor_facet_or_an_unknown_name() {
        for floor in NEVER_LIFTABLE {
            let mut policy = PublicationLanePolicy::as_shipped();
            policy.liftable_facets.push(floor.to_string());
            policy.liftable_facets.sort();
            assert!(policy.validate().is_err(), "{floor}");
        }
        let mut unknown = PublicationLanePolicy::as_shipped();
        unknown.liftable_facets.push("zzz_not_a_facet".to_string());
        unknown.liftable_facets.sort();
        assert!(unknown.validate().is_err());
        let mut unsorted = PublicationLanePolicy::as_shipped();
        unsorted.liftable_facets.reverse();
        assert!(unsorted.validate().is_err());
    }

    #[test]
    fn a_named_entry_may_name_only_facets_on_the_liftable_list() {
        let mut policy = with(named(&[], &["bundle_events"]));
        policy.validate().unwrap();
        policy
            .liftable_facets
            .retain(|facet| facet != "bundle_events");
        assert!(policy.validate().is_err());
    }

    #[test]
    fn a_named_bundle_lifts_exact_kinds_and_liftable_facets() {
        let policy = with(named(
            &["service", "view"],
            &["bundle_events", "owns_namespaces"],
        ));
        policy.validate().unwrap();
        let grants = policy.grants_for("demo");
        assert_eq!(grants.kinds, ["service", "view"]);
        assert_eq!(grants.facets, [Facet::BundleEvents, Facet::OwnsNamespaces]);
        assert!(policy.grants_for("other").kinds.is_empty());
    }

    #[test]
    fn facets_a_policy_can_never_lift_are_refused() {
        for facet in [
            "operator_vault",
            "state_home",
            "route_capabilities",
            "executors",
            "isolation_backends",
            "external_providers",
            "external_lifecycle_adapters",
            "provides_kinds",
        ] {
            assert!(with(named(&[], &[facet])).validate().is_err(), "{facet}");
        }
    }

    #[test]
    fn bad_names_and_unsorted_lists_are_refused() {
        assert!(with(named(&["Service"], &[])).validate().is_err());
        assert!(with(named(&["a b"], &[])).validate().is_err());
        assert!(with(named(&["view", "service"], &[])).validate().is_err());
        assert!(with(named(&["view", "view"], &[])).validate().is_err());
        let mut policy = PublicationLanePolicy::as_shipped();
        policy
            .named_bundles
            .insert("Bad Name".to_string(), NamedBundleLane::default());
        assert!(policy.validate().is_err());
        let mut schema = PublicationLanePolicy::as_shipped();
        schema.schema = 2;
        assert!(schema.validate().is_err());
    }

    #[test]
    fn unknown_fields_are_refused_and_a_kind_this_node_lacks_is_accepted() {
        let unknown = serde_json::json!({
            "schema": 1,
            "admit_activations": ["invoked"],
            "named_bundles": {"demo": {"kinds": [], "facets": [], "surprise": 1}},
        });
        assert!(serde_json::from_value::<PublicationLanePolicy>(unknown).is_err());
        with(named(&["a_kind_not_installed_here"], &[]))
            .validate()
            .unwrap();
    }
}
