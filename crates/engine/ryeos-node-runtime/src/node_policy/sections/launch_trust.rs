//! Node-owned admission of launch-root trust classes beyond each kind's
//! default.
//!
//! A kind schema declares the trust classes its launch roots carry by default
//! and which further classes an operator may admit. This section is where the
//! operator admits them, per kind, in the node-signed policy generation. It can
//! only widen within what each kind allows: the gates intersect it with the
//! kind's `operator_may_admit`, and a kind that does not list a class gains
//! nothing from it. Unsigned work is never admissible.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use ryeos_engine::resolution::TrustClass;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "launch_trust";
const MAX_KINDS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeLaunchTrustPolicy {
    pub schema: u32,
    /// Kind name → trust classes the operator admits for launch roots of
    /// that kind, in addition to the kind's default.
    pub admit: BTreeMap<String, Vec<TrustClass>>,
}

impl NodeLaunchTrustPolicy {
    /// The shipped default: the operator admits nothing beyond each kind's
    /// default.
    pub fn admitting_nothing() -> Self {
        Self {
            schema: 1,
            admit: BTreeMap::new(),
        }
    }

    /// A policy admitting `classes` for each named kind. Unvalidated; used to
    /// build exact snapshots in tests.
    pub fn admitting(entries: &[(&str, &[TrustClass])]) -> Self {
        Self {
            schema: 1,
            admit: entries
                .iter()
                .map(|(kind, classes)| ((*kind).to_owned(), classes.to_vec()))
                .collect(),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("node launch_trust policy schema is not current");
        }
        if self.admit.len() > MAX_KINDS {
            bail!("node launch_trust policy names more than {MAX_KINDS} kinds");
        }
        for (kind, classes) in &self.admit {
            let canonical = !kind.is_empty()
                && kind.len() <= 64
                && kind
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
                && kind
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_lowercase());
            if !canonical {
                bail!("node launch_trust policy names a non-canonical kind `{kind}`");
            }
            for (index, class) in classes.iter().enumerate() {
                if classes[..index].contains(class) {
                    bail!("node launch_trust policy admits a class twice for kind `{kind}`");
                }
                if *class != TrustClass::UntrustedProject {
                    bail!(
                        "node launch_trust policy may admit only `untrusted_project`; \
                         kind `{kind}` lists another class"
                    );
                }
            }
        }
        Ok(())
    }

    /// Refuse a policy that admits anything the installed kinds do not let an
    /// operator admit, or that names a kind which is not launched as a root.
    /// Run at boot, once the kind registry is loaded, so an entry that would
    /// have no effect is an error rather than a silent drop.
    pub fn check_against_kinds(
        &self,
        kinds: &ryeos_engine::kind_registry::KindRegistry,
    ) -> anyhow::Result<()> {
        for (kind, classes) in &self.admit {
            let schema = kinds
                .get(kind)
                .with_context(|| format!("node launch_trust policy names unknown kind `{kind}`"))?;
            let declared = schema.launch_trust.as_ref().with_context(|| {
                format!(
                    "node launch_trust policy names kind `{kind}`, which is not launched as a root"
                )
            })?;
            for class in classes {
                if !declared.operator_may_admit.contains(class) {
                    bail!(
                        "node launch_trust policy admits a class for kind `{kind}` that its kind schema \
                         does not let an operator admit"
                    );
                }
            }
        }
        Ok(())
    }

    /// The classes the operator admits for `kind`; empty when the policy does
    /// not name it.
    pub fn admitted_for(&self, kind: &str) -> &[TrustClass] {
        self.admit.get(kind).map(Vec::as_slice).unwrap_or(&[])
    }
}

impl TypedNodePolicy for NodeLaunchTrustPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct NodeLaunchTrustPolicySection;

impl NodePolicySection for NodeLaunchTrustPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: NodeLaunchTrustPolicy =
            serde_json::from_value(body.clone()).context("parse node launch_trust policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: Value) -> anyhow::Result<NodeLaunchTrustPolicy> {
        let record: NodeLaunchTrustPolicy = serde_json::from_value(body)?;
        record.validate()?;
        Ok(record)
    }

    #[test]
    fn default_body_admits_nothing() {
        let policy = parse(serde_json::json!({"schema": 1, "admit": {}})).unwrap();
        assert!(policy.admitted_for("tool").is_empty());
    }

    #[test]
    fn an_operator_may_admit_unpinned_signers_per_kind() {
        let policy = parse(serde_json::json!({
            "schema": 1,
            "admit": {"tool": ["untrusted_project"]}
        }))
        .unwrap();
        assert_eq!(policy.admitted_for("tool"), [TrustClass::UntrustedProject]);
        assert!(policy.admitted_for("knowledge").is_empty());
    }

    #[test]
    fn unsigned_and_other_classes_are_refused() {
        for class in ["unsigned", "trusted_bundle", "trusted_node"] {
            assert!(
                parse(serde_json::json!({"schema": 1, "admit": {"tool": [class]}})).is_err(),
                "{class}"
            );
        }
    }

    #[test]
    fn malformed_shapes_are_refused() {
        assert!(parse(serde_json::json!({"schema": 2, "admit": {}})).is_err());
        assert!(parse(serde_json::json!({"schema": 1, "admit": {"Tool": []}})).is_err());
        assert!(
            parse(serde_json::json!({
                "schema": 1,
                "admit": {"tool": ["untrusted_project", "untrusted_project"]}
            }))
            .is_err()
        );
        assert!(parse(serde_json::json!({"schema": 1, "admit": {}, "extra": true})).is_err());
    }
}
