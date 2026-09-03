//! Bounded comparison of retained admitted execution realizations.
//!
//! The retained object is kind-neutral. Comparison therefore reports only
//! mechanical tranches, validated content identities, and closed storage
//! variants. Contract-owned component roles and property vocabulary are used
//! for alignment inside the authorized process but never disclosed or hashed
//! into the response.

use std::collections::{BTreeMap, BTreeSet};

use ryeos_state::objects::{
    AdmittedExecutionRealization, ExecutionComponentReference, ExecutionComponentStorage,
};
use serde::Serialize;

use super::resolution::{
    DefinitionChangeKind, DefinitionValueSummary, DefinitionValueType,
    MAX_IDENTITY_COORDINATE_BYTES, MAX_IDENTITY_DIFF_VISITS, MAX_PUBLIC_SCALAR_BYTES,
};

pub const MAX_COMPARISON_CHANGE_ROWS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRealizationTranche {
    SubstrateIdentity,
    SubstrateAttestation,
    LaunchAuthority,
    EffectiveDefinition,
    ArtifactIdentity,
    ExecutionClosure,
    ExecutionContract,
    Components,
    Properties,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRealizationChange {
    pub tranche: ExecutionRealizationTranche,
    pub coordinate: String,
    pub change: DefinitionChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<DefinitionValueSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right: Option<DefinitionValueSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRealizationComparison {
    pub left_hash: String,
    pub right_hash: String,
    pub changed: bool,
    pub complete: bool,
    pub tranche_changes: Vec<ExecutionRealizationChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionRealizationComparisonError(String);

impl std::fmt::Display for ExecutionRealizationComparisonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ExecutionRealizationComparisonError {}

impl ExecutionRealizationComparison {
    /// Compare two independently verified current admitted realizations.
    ///
    /// `maximum_rows` is the part of the shared comparison field budget left
    /// after definition changes have been emitted.
    pub fn between(
        left_hash: &str,
        left: &AdmittedExecutionRealization,
        right_hash: &str,
        right: &AdmittedExecutionRealization,
        maximum_rows: usize,
    ) -> Result<Self, ExecutionRealizationComparisonError> {
        verify_operand("left", left_hash, left)?;
        verify_operand("right", right_hash, right)?;

        if left_hash == right_hash {
            return Ok(Self {
                left_hash: left_hash.to_string(),
                right_hash: right_hash.to_string(),
                changed: false,
                complete: true,
                tranche_changes: Vec::new(),
            });
        }

        let mut builder = RealizationDiffBuilder::new(maximum_rows);
        builder.diff_public_hash(
            ExecutionRealizationTranche::SubstrateIdentity,
            "substrate_identity_hash",
            &left.substrate_identity_hash,
            &right.substrate_identity_hash,
        );
        builder.diff_public_hash(
            ExecutionRealizationTranche::SubstrateAttestation,
            "substrate_attestation_hash",
            &left.substrate_attestation_hash,
            &right.substrate_attestation_hash,
        );
        builder.diff_public_hash(
            ExecutionRealizationTranche::LaunchAuthority,
            "launch_authority_digest",
            &left.launch_authority_digest,
            &right.launch_authority_digest,
        );
        builder.diff_public_hash(
            ExecutionRealizationTranche::EffectiveDefinition,
            "effective_definition_digest",
            &left.effective_definition_digest,
            &right.effective_definition_digest,
        );
        builder.diff_public_hash(
            ExecutionRealizationTranche::ArtifactIdentity,
            "artifact_identity_digest",
            &left.artifact_identity_digest,
            &right.artifact_identity_digest,
        );
        builder.diff_public_hash(
            ExecutionRealizationTranche::ExecutionClosure,
            "execution_closure_digest",
            &left.execution_closure_digest,
            &right.execution_closure_digest,
        );
        builder.diff_private_string(
            ExecutionRealizationTranche::ExecutionContract,
            "execution_contract.ref",
            &left.contract_ref,
            &right.contract_ref,
        );
        builder.diff_public_hash(
            ExecutionRealizationTranche::ExecutionContract,
            "execution_contract.digest",
            &left.contract_digest,
            &right.contract_digest,
        );
        builder.diff_components(&left.components, &right.components);
        builder.diff_properties(&left.properties, &right.properties);

        Ok(builder.finish(left_hash, right_hash))
    }
}

fn verify_operand(
    side: &str,
    expected_hash: &str,
    realization: &AdmittedExecutionRealization,
) -> Result<(), ExecutionRealizationComparisonError> {
    realization.validate().map_err(|error| {
        ExecutionRealizationComparisonError(format!(
            "{side} admitted execution realization is invalid: {error}"
        ))
    })?;
    let observed = realization.content_hash().map_err(|error| {
        ExecutionRealizationComparisonError(format!(
            "compute {side} admitted execution realization identity: {error}"
        ))
    })?;
    if observed != expected_hash {
        return Err(ExecutionRealizationComparisonError(format!(
            "{side} admitted execution realization hash does not match retained content"
        )));
    }
    Ok(())
}

struct RealizationDiffBuilder {
    maximum_rows: usize,
    visits: usize,
    complete: bool,
    halted: bool,
    changes: Vec<ExecutionRealizationChange>,
}

impl RealizationDiffBuilder {
    fn new(maximum_rows: usize) -> Self {
        Self {
            maximum_rows: maximum_rows.min(MAX_COMPARISON_CHANGE_ROWS),
            visits: 0,
            complete: true,
            halted: false,
            changes: Vec::new(),
        }
    }

    fn visit(&mut self) -> bool {
        if self.halted {
            return false;
        }
        if self.visits >= MAX_IDENTITY_DIFF_VISITS {
            self.complete = false;
            self.halted = true;
            return false;
        }
        self.visits += 1;
        true
    }

    fn push(
        &mut self,
        tranche: ExecutionRealizationTranche,
        coordinate: String,
        change: DefinitionChangeKind,
        left: Option<DefinitionValueSummary>,
        right: Option<DefinitionValueSummary>,
    ) {
        if self.halted {
            return;
        }
        if coordinate.len() > MAX_IDENTITY_COORDINATE_BYTES {
            self.complete = false;
            return;
        }
        if self.changes.len() >= self.maximum_rows {
            self.complete = false;
            self.halted = true;
            return;
        }
        self.changes.push(ExecutionRealizationChange {
            tranche,
            coordinate,
            change,
            left,
            right,
        });
    }

    fn finish(mut self, left_hash: &str, right_hash: &str) -> ExecutionRealizationComparison {
        self.changes.sort_by(|left, right| {
            (left.tranche, &left.coordinate, left.change).cmp(&(
                right.tranche,
                &right.coordinate,
                right.change,
            ))
        });
        ExecutionRealizationComparison {
            left_hash: left_hash.to_string(),
            right_hash: right_hash.to_string(),
            changed: true,
            complete: self.complete,
            tranche_changes: self.changes,
        }
    }

    fn diff_public_hash(
        &mut self,
        tranche: ExecutionRealizationTranche,
        coordinate: &str,
        left: &str,
        right: &str,
    ) {
        if left == right || !self.visit() {
            return;
        }
        self.push(
            tranche,
            coordinate.to_string(),
            DefinitionChangeKind::Changed,
            Some(public_hash(left)),
            Some(public_hash(right)),
        );
    }

    fn diff_private_string(
        &mut self,
        tranche: ExecutionRealizationTranche,
        coordinate: &str,
        left: &str,
        right: &str,
    ) {
        if left == right || !self.visit() {
            return;
        }
        self.push(
            tranche,
            coordinate.to_string(),
            DefinitionChangeKind::Changed,
            Some(private_summary(DefinitionValueType::String)),
            Some(private_summary(DefinitionValueType::String)),
        );
    }

    fn diff_components(
        &mut self,
        left: &[ExecutionComponentReference],
        right: &[ExecutionComponentReference],
    ) {
        let left = left
            .iter()
            .map(|component| (component.role.as_str(), component))
            .collect::<BTreeMap<_, _>>();
        let right = right
            .iter()
            .map(|component| (component.role.as_str(), component))
            .collect::<BTreeMap<_, _>>();
        let roles = left
            .keys()
            .chain(right.keys())
            .copied()
            .collect::<BTreeSet<_>>();

        for (slot, role) in roles.into_iter().enumerate() {
            if self.halted || !self.visit() {
                break;
            }
            let coordinate = format!("components.slot[{slot:04}]");
            match (left.get(role), right.get(role)) {
                (None, Some(_)) => self.push(
                    ExecutionRealizationTranche::Components,
                    coordinate,
                    DefinitionChangeKind::Added,
                    None,
                    Some(private_summary(DefinitionValueType::Object)),
                ),
                (Some(_), None) => self.push(
                    ExecutionRealizationTranche::Components,
                    coordinate,
                    DefinitionChangeKind::Removed,
                    Some(private_summary(DefinitionValueType::Object)),
                    None,
                ),
                (Some(left), Some(right)) => {
                    self.diff_public_hash(
                        ExecutionRealizationTranche::Components,
                        &format!("{coordinate}.content_digest"),
                        &left.content_digest,
                        &right.content_digest,
                    );
                    self.diff_component_storage(&coordinate, &left.material, &right.material);
                }
                (None, None) => {}
            }
        }
    }

    fn diff_component_storage(
        &mut self,
        coordinate: &str,
        left: &ExecutionComponentStorage,
        right: &ExecutionComponentStorage,
    ) {
        let left_kind = storage_kind(left);
        let right_kind = storage_kind(right);
        if left_kind != right_kind && self.visit() {
            self.push(
                ExecutionRealizationTranche::Components,
                format!("{coordinate}.storage.kind"),
                DefinitionChangeKind::Changed,
                Some(public_closed_string(left_kind)),
                Some(public_closed_string(right_kind)),
            );
            return;
        }
        match (left, right) {
            (
                ExecutionComponentStorage::CasObject {
                    hash: left_hash,
                    expected_kind: left_kind,
                },
                ExecutionComponentStorage::CasObject {
                    hash: right_hash,
                    expected_kind: right_kind,
                },
            ) => {
                self.diff_public_hash(
                    ExecutionRealizationTranche::Components,
                    &format!("{coordinate}.storage.hash"),
                    left_hash,
                    right_hash,
                );
                self.diff_private_string(
                    ExecutionRealizationTranche::Components,
                    &format!("{coordinate}.storage.expected_kind"),
                    left_kind,
                    right_kind,
                );
            }
            (
                ExecutionComponentStorage::CasBlob { hash: left },
                ExecutionComponentStorage::CasBlob { hash: right },
            ) => self.diff_public_hash(
                ExecutionRealizationTranche::Components,
                &format!("{coordinate}.storage.hash"),
                left,
                right,
            ),
            (
                ExecutionComponentStorage::LargeObject {
                    hash: left_hash,
                    bytes: left_bytes,
                },
                ExecutionComponentStorage::LargeObject {
                    hash: right_hash,
                    bytes: right_bytes,
                },
            ) => {
                self.diff_public_hash(
                    ExecutionRealizationTranche::Components,
                    &format!("{coordinate}.storage.hash"),
                    left_hash,
                    right_hash,
                );
                if left_bytes != right_bytes && self.visit() {
                    self.push(
                        ExecutionRealizationTranche::Components,
                        format!("{coordinate}.storage.bytes"),
                        DefinitionChangeKind::Changed,
                        Some(private_summary(DefinitionValueType::Number)),
                        Some(private_summary(DefinitionValueType::Number)),
                    );
                }
            }
            _ => {}
        }
    }

    fn diff_properties(
        &mut self,
        left: &BTreeMap<String, serde_json::Value>,
        right: &BTreeMap<String, serde_json::Value>,
    ) {
        let keys = left
            .keys()
            .chain(right.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for (slot, key) in keys.into_iter().enumerate() {
            if self.halted || !self.visit() {
                break;
            }
            let coordinate = format!("properties.slot[{slot:04}]");
            match (left.get(&key), right.get(&key)) {
                (None, Some(right)) => self.push(
                    ExecutionRealizationTranche::Properties,
                    coordinate,
                    DefinitionChangeKind::Added,
                    None,
                    Some(private_summary(json_type(right))),
                ),
                (Some(left), None) => self.push(
                    ExecutionRealizationTranche::Properties,
                    coordinate,
                    DefinitionChangeKind::Removed,
                    Some(private_summary(json_type(left))),
                    None,
                ),
                (Some(left), Some(right)) if left != right => self.push(
                    ExecutionRealizationTranche::Properties,
                    coordinate,
                    DefinitionChangeKind::Changed,
                    Some(private_summary(json_type(left))),
                    Some(private_summary(json_type(right))),
                ),
                _ => {}
            }
        }
    }
}

fn storage_kind(storage: &ExecutionComponentStorage) -> &'static str {
    match storage {
        ExecutionComponentStorage::CasObject { .. } => "cas_object",
        ExecutionComponentStorage::CasBlob { .. } => "cas_blob",
        ExecutionComponentStorage::LargeObject { .. } => "large_object",
    }
}

fn public_hash(value: &str) -> DefinitionValueSummary {
    debug_assert!(value.len() <= MAX_PUBLIC_SCALAR_BYTES);
    DefinitionValueSummary {
        value_type: DefinitionValueType::String,
        public_scalar: Some(value.to_string()),
    }
}

fn public_closed_string(value: &str) -> DefinitionValueSummary {
    DefinitionValueSummary {
        value_type: DefinitionValueType::String,
        public_scalar: Some(value.to_string()),
    }
}

fn private_summary(value_type: DefinitionValueType) -> DefinitionValueSummary {
    DefinitionValueSummary {
        value_type,
        public_scalar: None,
    }
}

fn json_type(value: &serde_json::Value) -> DefinitionValueType {
    match value {
        serde_json::Value::Null => DefinitionValueType::Null,
        serde_json::Value::Bool(_) => DefinitionValueType::Boolean,
        serde_json::Value::Number(_) => DefinitionValueType::Number,
        serde_json::Value::String(_) => DefinitionValueType::String,
        serde_json::Value::Array(_) => DefinitionValueType::Array,
        serde_json::Value::Object(_) => DefinitionValueType::Object,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::objects::{
        ADMITTED_EXECUTION_REALIZATION_KIND, EXECUTION_REALIZATION_SCHEMA_VERSION,
    };

    fn realization() -> AdmittedExecutionRealization {
        AdmittedExecutionRealization {
            schema: EXECUTION_REALIZATION_SCHEMA_VERSION,
            kind: ADMITTED_EXECUTION_REALIZATION_KIND.to_string(),
            substrate_identity_hash: "a".repeat(64),
            substrate_attestation_hash: "b".repeat(64),
            launch_authority_digest: "c".repeat(64),
            effective_definition_digest: "d".repeat(64),
            artifact_identity_digest: "e".repeat(64),
            execution_closure_digest: "f".repeat(64),
            contract_ref: "execution:test/fixture".to_string(),
            contract_digest: "1".repeat(64),
            components: vec![ExecutionComponentReference {
                role: "runtime".to_string(),
                content_digest: "2".repeat(64),
                material: ExecutionComponentStorage::CasBlob {
                    hash: "3".repeat(64),
                },
            }],
            properties: BTreeMap::new(),
        }
    }

    #[test]
    fn equal_realizations_are_complete_and_empty() {
        let value = realization();
        let hash = value.content_hash().unwrap();
        let comparison =
            ExecutionRealizationComparison::between(&hash, &value, &hash, &value, 512).unwrap();
        assert!(!comparison.changed);
        assert!(comparison.complete);
        assert!(comparison.tranche_changes.is_empty());
    }

    #[test]
    fn same_definition_under_different_realizations_is_visible() {
        let left = realization();
        let mut right = realization();
        right.substrate_identity_hash = "9".repeat(64);
        let left_hash = left.content_hash().unwrap();
        let right_hash = right.content_hash().unwrap();

        let comparison =
            ExecutionRealizationComparison::between(&left_hash, &left, &right_hash, &right, 512)
                .unwrap();
        assert!(comparison.changed);
        assert!(comparison.complete);
        assert_eq!(comparison.tranche_changes.len(), 1);
        assert_eq!(
            comparison.tranche_changes[0].tranche,
            ExecutionRealizationTranche::SubstrateIdentity
        );
    }

    #[test]
    fn component_roles_properties_and_their_hashes_never_escape() {
        let left_secret = "private-role-left";
        let right_secret = "private-role-right";
        let mut left = realization();
        let mut right = realization();
        left.components[0].role = left_secret.to_string();
        right.components[0].role = right_secret.to_string();
        left.properties.insert(
            "operator/private-left".to_string(),
            serde_json::json!(left_secret),
        );
        right.properties.insert(
            "operator/private-right".to_string(),
            serde_json::json!(right_secret),
        );
        let left_hash = left.content_hash().unwrap();
        let right_hash = right.content_hash().unwrap();
        let comparison =
            ExecutionRealizationComparison::between(&left_hash, &left, &right_hash, &right, 512)
                .unwrap();
        let encoded = serde_json::to_string(&comparison).unwrap();
        for forbidden in [
            left_secret,
            right_secret,
            "operator/private-left",
            "operator/private-right",
        ] {
            assert!(!encoded.contains(forbidden), "leaked {forbidden}");
            assert!(!encoded.contains(&lillux::sha256_hex(forbidden.as_bytes())));
        }
    }

    #[test]
    fn shared_row_budget_never_claims_completeness() {
        let left = realization();
        let mut right = realization();
        right.substrate_identity_hash = "9".repeat(64);
        let left_hash = left.content_hash().unwrap();
        let right_hash = right.content_hash().unwrap();
        let comparison =
            ExecutionRealizationComparison::between(&left_hash, &left, &right_hash, &right, 0)
                .unwrap();
        assert!(comparison.changed);
        assert!(!comparison.complete);
        assert!(comparison.tranche_changes.is_empty());
    }

    #[test]
    fn retained_hash_mismatch_refuses() {
        let value = realization();
        let error = ExecutionRealizationComparison::between(
            &"0".repeat(64),
            &value,
            &value.content_hash().unwrap(),
            &value,
            512,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match retained content")
        );
    }
}
