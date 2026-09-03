//! Kind-neutral retained execution-realization evidence.
//!
//! Owning execution contracts assign meaning to component roles and property
//! keys. State validates bounded canonical structure and exposes mechanical
//! CAS/blob/large-object edges; it never interprets model, tokenizer,
//! interpreter, backend, or checkpoint vocabulary.

use std::collections::BTreeMap;

use anyhow::bail;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const ADMITTED_EXECUTION_REALIZATION_KIND: &str = "admitted_execution_realization";
pub const OBSERVED_EXECUTION_REALIZATION_KIND: &str = "observed_execution_realization";
pub const EXECUTION_REALIZATION_SCHEMA_VERSION: u32 = 1;
pub const MAX_EXECUTION_REALIZATION_BYTES: usize = 256 * 1024;
pub const MAX_EXECUTION_COMPONENTS: usize = 256;
pub const MAX_EXECUTION_PROPERTIES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "storage", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionComponentStorage {
    CasObject { hash: String, expected_kind: String },
    CasBlob { hash: String },
    LargeObject { hash: String, bytes: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionComponentReference {
    /// Opaque identifier interpreted only by the owning execution contract.
    pub role: String,
    pub content_digest: String,
    pub material: ExecutionComponentStorage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedExecutionRealization {
    pub schema: u32,
    pub kind: String,
    pub substrate_identity_hash: String,
    pub substrate_attestation_hash: String,
    /// Digest of the capsule fields that exist before this object's hash is
    /// known. The capsule recomputes it, which avoids an impossible CAS cycle.
    pub launch_authority_digest: String,
    pub effective_definition_digest: String,
    pub artifact_identity_digest: String,
    pub execution_closure_digest: String,
    pub contract_ref: String,
    pub contract_digest: String,
    pub components: Vec<ExecutionComponentReference>,
    /// Opaque, bounded mechanical policy facts owned by `contract_ref`.
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedExecutionRealization {
    pub schema: u32,
    pub kind: String,
    pub admitted_realization_hash: String,
    pub contract_ref: String,
    pub contract_digest: String,
    pub components: Vec<ExecutionComponentReference>,
    pub properties: BTreeMap<String, Value>,
}

impl ExecutionComponentReference {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_opaque_name("execution component role", &self.role)?;
        require_hash("execution component content digest", &self.content_digest)?;
        match &self.material {
            ExecutionComponentStorage::CasObject {
                hash,
                expected_kind,
            } => {
                require_hash("execution component object hash", hash)?;
                validate_opaque_name("execution component expected kind", expected_kind)?;
            }
            ExecutionComponentStorage::CasBlob { hash } => {
                require_hash("execution component blob hash", hash)?;
            }
            ExecutionComponentStorage::LargeObject { hash, bytes } => {
                require_hash("execution component large-object hash", hash)?;
                if *bytes == 0 {
                    bail!("execution component large-object byte count must be positive");
                }
            }
        }
        Ok(())
    }
}

impl AdmittedExecutionRealization {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_identity(self.schema, &self.kind, ADMITTED_EXECUTION_REALIZATION_KIND)?;
        for (field, hash) in [
            ("substrate identity hash", &self.substrate_identity_hash),
            (
                "substrate attestation hash",
                &self.substrate_attestation_hash,
            ),
            ("launch authority digest", &self.launch_authority_digest),
            (
                "effective definition digest",
                &self.effective_definition_digest,
            ),
            ("artifact identity digest", &self.artifact_identity_digest),
            ("execution closure digest", &self.execution_closure_digest),
            ("execution contract digest", &self.contract_digest),
        ] {
            require_hash(field, hash)?;
        }
        validate_opaque_name("execution contract ref", &self.contract_ref)?;
        validate_components(&self.components)?;
        validate_properties(&self.properties)?;
        validate_size(self)
    }

    pub fn from_current_value(value: &Value) -> anyhow::Result<Self> {
        require_current_outer(value, ADMITTED_EXECUTION_REALIZATION_KIND)?;
        let realization: Self = serde_json::from_value(value.clone())?;
        realization.validate()?;
        Ok(realization)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn content_hash(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_digest(self)
    }

    pub fn verify_retained_components(
        &self,
        cas: &lillux::CasStore,
        large_store: &crate::large_object_store::LargeObjectStore,
    ) -> anyhow::Result<()> {
        verify_retained_components(&self.components, cas, large_store)
    }
}

impl ObservedExecutionRealization {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_identity(self.schema, &self.kind, OBSERVED_EXECUTION_REALIZATION_KIND)?;
        require_hash(
            "admitted execution realization hash",
            &self.admitted_realization_hash,
        )?;
        validate_opaque_name("execution observation contract ref", &self.contract_ref)?;
        require_hash(
            "execution observation contract digest",
            &self.contract_digest,
        )?;
        validate_components(&self.components)?;
        validate_properties(&self.properties)?;
        validate_size(self)
    }

    pub fn from_current_value(value: &Value) -> anyhow::Result<Self> {
        require_current_outer(value, OBSERVED_EXECUTION_REALIZATION_KIND)?;
        let realization: Self = serde_json::from_value(value.clone())?;
        realization.validate()?;
        Ok(realization)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn content_hash(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_digest(self)
    }

    pub fn verify_retained_components(
        &self,
        cas: &lillux::CasStore,
        large_store: &crate::large_object_store::LargeObjectStore,
    ) -> anyhow::Result<()> {
        verify_retained_components(&self.components, cas, large_store)
    }
}

fn verify_retained_components(
    components: &[ExecutionComponentReference],
    cas: &lillux::CasStore,
    large_store: &crate::large_object_store::LargeObjectStore,
) -> anyhow::Result<()> {
    for component in components {
        match &component.material {
            ExecutionComponentStorage::CasObject {
                hash,
                expected_kind,
            } => {
                let value = cas.get_object(hash)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "execution component `{}` object {hash} is missing",
                        component.role
                    )
                })?;
                let observed = value.get("kind").and_then(Value::as_str).ok_or_else(|| {
                    anyhow::anyhow!(
                        "execution component `{}` object has no kind",
                        component.role
                    )
                })?;
                if observed != expected_kind {
                    bail!(
                        "execution component `{}` expected object kind `{expected_kind}`, observed `{observed}`",
                        component.role
                    );
                }
            }
            ExecutionComponentStorage::CasBlob { hash } => {
                if cas.get_blob(hash)?.is_none() {
                    bail!(
                        "execution component `{}` blob {hash} is missing",
                        component.role
                    );
                }
            }
            ExecutionComponentStorage::LargeObject { hash, bytes } => {
                large_store.verify_resident_object(hash, *bytes)?;
            }
        }
    }
    Ok(())
}

fn validate_identity(schema: u32, kind: &str, expected_kind: &str) -> anyhow::Result<()> {
    if schema != EXECUTION_REALIZATION_SCHEMA_VERSION {
        bail!("execution realization schema is not current");
    }
    if kind != expected_kind {
        bail!("unexpected execution realization kind: {kind}");
    }
    Ok(())
}

fn require_current_outer(value: &Value, expected_kind: &'static str) -> anyhow::Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("execution realization must be an object"))?;
    let kind = object.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind != expected_kind {
        bail!("unexpected execution realization kind: {kind}");
    }
    let schema = object.get("schema").and_then(Value::as_u64).unwrap_or(0);
    if schema != u64::from(EXECUTION_REALIZATION_SCHEMA_VERSION) {
        return Err(super::IncompatibleCurrentObjectSchema::new(
            expected_kind,
            schema,
            EXECUTION_REALIZATION_SCHEMA_VERSION,
        )
        .into());
    }
    Ok(())
}

fn validate_components(components: &[ExecutionComponentReference]) -> anyhow::Result<()> {
    if components.len() > MAX_EXECUTION_COMPONENTS {
        bail!("execution realization has too many components");
    }
    let mut prior: Option<&str> = None;
    for component in components {
        component.validate()?;
        if prior.is_some_and(|value| value >= component.role.as_str()) {
            bail!("execution realization components must be sorted and unique by role");
        }
        prior = Some(&component.role);
    }
    Ok(())
}

fn validate_properties(properties: &BTreeMap<String, Value>) -> anyhow::Result<()> {
    if properties.len() > MAX_EXECUTION_PROPERTIES {
        bail!("execution realization has too many properties");
    }
    for (key, value) in properties {
        validate_opaque_name("execution realization property", key)?;
        if value.is_array() || value.is_object() {
            bail!("execution realization properties must be scalar or null");
        }
    }
    Ok(())
}

fn validate_opaque_name(field: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 255
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/' | b':')
        })
    {
        bail!("{field} has a non-canonical value: {value:?}");
    }
    Ok(())
}

fn require_hash(field: &str, value: &str) -> anyhow::Result<()> {
    super::thread_snapshot::validate_canonical_hash(field, value)
}

fn validate_size<T: Serialize>(value: &T) -> anyhow::Result<()> {
    let bytes = lillux::canonical_json(&serde_json::to_value(value)?)?.len();
    if bytes > MAX_EXECUTION_REALIZATION_BYTES {
        bail!("execution realization exceeds {MAX_EXECUTION_REALIZATION_BYTES} bytes");
    }
    Ok(())
}

fn canonical_digest<T: Serialize>(value: &T) -> anyhow::Result<String> {
    Ok(lillux::sha256_hex(
        lillux::canonical_json(&serde_json::to_value(value)?)?.as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admitted() -> AdmittedExecutionRealization {
        AdmittedExecutionRealization {
            schema: EXECUTION_REALIZATION_SCHEMA_VERSION,
            kind: ADMITTED_EXECUTION_REALIZATION_KIND.to_owned(),
            substrate_identity_hash: "a".repeat(64),
            substrate_attestation_hash: "b".repeat(64),
            launch_authority_digest: "c".repeat(64),
            effective_definition_digest: "d".repeat(64),
            artifact_identity_digest: "e".repeat(64),
            execution_closure_digest: "f".repeat(64),
            contract_ref: "execution:test/fixture".to_owned(),
            contract_digest: "1".repeat(64),
            components: vec![ExecutionComponentReference {
                role: "fixture".to_owned(),
                content_digest: "2".repeat(64),
                material: ExecutionComponentStorage::CasBlob {
                    hash: "3".repeat(64),
                },
            }],
            properties: BTreeMap::new(),
        }
    }

    #[test]
    fn admitted_realization_round_trips_and_moves_with_components() {
        let value = admitted().to_value().unwrap();
        assert_eq!(
            AdmittedExecutionRealization::from_current_value(&value).unwrap(),
            admitted()
        );
        let digest = admitted().content_hash().unwrap();
        let mut changed = admitted();
        changed.components[0].content_digest = "4".repeat(64);
        assert_ne!(digest, changed.content_hash().unwrap());
    }

    #[test]
    fn unsorted_components_and_predecessor_schema_refuse() {
        let mut realization = admitted();
        realization.components.push(ExecutionComponentReference {
            role: "aaa".to_owned(),
            content_digest: "5".repeat(64),
            material: ExecutionComponentStorage::CasBlob {
                hash: "6".repeat(64),
            },
        });
        assert!(realization.validate().is_err());
        let mut value = admitted().to_value().unwrap();
        value["schema"] = serde_json::json!(0);
        assert!(AdmittedExecutionRealization::from_current_value(&value).is_err());
    }
}
