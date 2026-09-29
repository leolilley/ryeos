//! CAS-owned content links for deterministic guest-runtime materialization.
//!
//! These objects retain bytes; they are not admission or qualification
//! testimony. The node-signed attestation separately joins their hashes to an
//! authenticated operator, checked source generation and materializer policy.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const GUEST_RUNTIME_MATERIALIZATION_SOURCE_KIND: &str = "guest_runtime_materialization_source";
pub const GUEST_RUNTIME_MATERIALIZATION_SUBJECT_KIND: &str =
    "guest_runtime_materialization_subject";
pub const GUEST_RUNTIME_MATERIALIZATION_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializationSignedItem {
    pub resolved_ref: String,
    pub bundle_name: String,
    pub signer_fingerprint: String,
    /// Hash of the whole signed source envelope, retained as a CAS blob.
    pub signed_blob_hash: String,
    pub raw_content_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializationSignedBundleManifest {
    pub bundle_name: String,
    pub signer_fingerprint: String,
    pub signed_blob_hash: String,
    pub body_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializationExecutorSource {
    pub bundle_name: String,
    pub item_ref: String,
    pub target_triple: String,
    pub signer_fingerprint: String,
    pub signed_manifest_ref_blob_hash: String,
    /// Canonical bytes of the signed-ref-selected source manifest, copied to a
    /// blob so this one-runtime closure does not retain every unrelated
    /// executor named by the complete Bundle manifest object.
    pub manifest_object_blob_hash: String,
    pub item_source_object_hash: String,
    pub signed_sidecar_blob_hash: String,
    /// Exact executable bytes. The output manifest may reach the same blob;
    /// equality is checked by the materialization publisher/reader.
    pub payload_blob_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestRuntimeMaterializationSourceEvidence {
    pub schema: u32,
    pub kind: String,
    pub signed_recipe_items: Vec<MaterializationSignedItem>,
    pub signed_bundle_manifests: Vec<MaterializationSignedBundleManifest>,
    pub executor: MaterializationExecutorSource,
}

impl GuestRuntimeMaterializationSourceEvidence {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let object: Self = serde_json::from_value(value.clone())?;
        object.validate()?;
        Ok(object)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        crate::objects::canonical_value_digest(&self.to_value()?)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != GUEST_RUNTIME_MATERIALIZATION_SCHEMA
            || self.kind != GUEST_RUNTIME_MATERIALIZATION_SOURCE_KIND
        {
            anyhow::bail!("guest-runtime materialization source schema/kind is not current");
        }
        if self.signed_recipe_items.is_empty() || self.signed_recipe_items.len() > 64 {
            anyhow::bail!("guest-runtime materialization recipe item count is invalid");
        }
        if self.signed_bundle_manifests.is_empty() || self.signed_bundle_manifests.len() > 8 {
            anyhow::bail!("guest-runtime materialization Bundle manifest count is invalid");
        }
        let mut previous_item: Option<(&str, &str)> = None;
        for item in &self.signed_recipe_items {
            require_label("recipe ref", &item.resolved_ref, 256)?;
            require_bundle_name(&item.bundle_name)?;
            for (label, hash) in [
                ("recipe item signer", &item.signer_fingerprint),
                ("signed recipe blob", &item.signed_blob_hash),
                ("recipe raw content", &item.raw_content_digest),
            ] {
                require_hash(label, hash)?;
            }
            let key = (item.resolved_ref.as_str(), item.bundle_name.as_str());
            if previous_item.is_some_and(|previous| previous >= key) {
                anyhow::bail!(
                    "guest-runtime materialization recipe items are not strictly ordered"
                );
            }
            previous_item = Some(key);
        }
        let mut previous_bundle: Option<&str> = None;
        for bundle in &self.signed_bundle_manifests {
            require_bundle_name(&bundle.bundle_name)?;
            for (label, hash) in [
                ("Bundle manifest signer", &bundle.signer_fingerprint),
                ("signed Bundle manifest", &bundle.signed_blob_hash),
                ("Bundle manifest body", &bundle.body_digest),
            ] {
                require_hash(label, hash)?;
            }
            if previous_bundle.is_some_and(|previous| previous >= bundle.bundle_name.as_str()) {
                anyhow::bail!(
                    "guest-runtime materialization Bundle manifests are not strictly ordered"
                );
            }
            previous_bundle = Some(&bundle.bundle_name);
        }
        for item in &self.signed_recipe_items {
            if !self
                .signed_bundle_manifests
                .iter()
                .any(|bundle| bundle.bundle_name == item.bundle_name)
            {
                anyhow::bail!("guest-runtime materialization recipe item has no Bundle manifest");
            }
        }
        let executor = &self.executor;
        require_bundle_name(&executor.bundle_name)?;
        require_label("executor item ref", &executor.item_ref, 256)?;
        require_target(&executor.target_triple)?;
        let expected_prefix = format!("bin/{}/", executor.target_triple);
        let Some(binary_name) = executor.item_ref.strip_prefix(&expected_prefix) else {
            anyhow::bail!("guest-runtime materialization executor item ref differs from target");
        };
        if binary_name.is_empty() || binary_name.contains('/') || binary_name.starts_with('.') {
            anyhow::bail!("guest-runtime materialization executor binary name is invalid");
        }
        if !self
            .signed_bundle_manifests
            .iter()
            .any(|bundle| bundle.bundle_name == executor.bundle_name)
        {
            anyhow::bail!("guest-runtime materialization executor has no Bundle manifest");
        }
        for bundle in &self.signed_bundle_manifests {
            if bundle.bundle_name != executor.bundle_name
                && !self
                    .signed_recipe_items
                    .iter()
                    .any(|item| item.bundle_name == bundle.bundle_name)
            {
                anyhow::bail!("guest-runtime materialization has an unrelated Bundle manifest");
            }
        }
        for (label, hash) in [
            ("executor signer", &executor.signer_fingerprint),
            (
                "signed executor manifest ref",
                &executor.signed_manifest_ref_blob_hash,
            ),
            (
                "executor manifest object blob",
                &executor.manifest_object_blob_hash,
            ),
            (
                "executor ItemSource object",
                &executor.item_source_object_hash,
            ),
            (
                "signed executor sidecar",
                &executor.signed_sidecar_blob_hash,
            ),
            ("executor payload blob", &executor.payload_blob_hash),
        ] {
            require_hash(label, hash)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestRuntimeMaterializationSubject {
    pub schema: u32,
    pub kind: String,
    pub coordinate_digest: String,
    pub runtime_manifest_hash: String,
    pub source_evidence_hash: String,
}

impl GuestRuntimeMaterializationSubject {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let object: Self = serde_json::from_value(value.clone())?;
        object.validate()?;
        Ok(object)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        crate::objects::canonical_value_digest(&self.to_value()?)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != GUEST_RUNTIME_MATERIALIZATION_SCHEMA
            || self.kind != GUEST_RUNTIME_MATERIALIZATION_SUBJECT_KIND
        {
            anyhow::bail!("guest-runtime materialization subject schema/kind is not current");
        }
        for (label, hash) in [
            ("materialization coordinate", &self.coordinate_digest),
            ("materialized runtime manifest", &self.runtime_manifest_hash),
            (
                "materialization source evidence",
                &self.source_evidence_hash,
            ),
        ] {
            require_hash(label, hash)?;
        }
        Ok(())
    }
}

fn require_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("{label} is not a canonical SHA-256 digest");
    }
    Ok(())
}

fn require_bundle_name(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        anyhow::bail!("guest-runtime materialization Bundle name is invalid");
    }
    Ok(())
}

fn require_target(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 96
        || value.starts_with('.')
        || value.contains("..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        anyhow::bail!("guest-runtime materialization executor target is invalid");
    }
    Ok(())
}

fn require_label(label: &str, value: &str, maximum_bytes: usize) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > maximum_bytes
        || value.starts_with('.')
        || value.contains("..")
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':')
        })
    {
        anyhow::bail!("guest-runtime materialization {label} is invalid");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn source() -> GuestRuntimeMaterializationSourceEvidence {
        GuestRuntimeMaterializationSourceEvidence {
            schema: GUEST_RUNTIME_MATERIALIZATION_SCHEMA,
            kind: GUEST_RUNTIME_MATERIALIZATION_SOURCE_KIND.to_owned(),
            signed_recipe_items: vec![MaterializationSignedItem {
                resolved_ref: "config:codex/guest-owner-materialization".to_owned(),
                bundle_name: "codex".to_owned(),
                signer_fingerprint: hash('a'),
                signed_blob_hash: hash('b'),
                raw_content_digest: hash('c'),
            }],
            signed_bundle_manifests: vec![MaterializationSignedBundleManifest {
                bundle_name: "codex".to_owned(),
                signer_fingerprint: hash('d'),
                signed_blob_hash: hash('e'),
                body_digest: hash('f'),
            }],
            executor: MaterializationExecutorSource {
                bundle_name: "codex".to_owned(),
                item_ref: "bin/x86_64-unknown-linux-gnu/owner".to_owned(),
                target_triple: "x86_64-unknown-linux-gnu".to_owned(),
                signer_fingerprint: hash('1'),
                signed_manifest_ref_blob_hash: hash('2'),
                manifest_object_blob_hash: hash('3'),
                item_source_object_hash: hash('4'),
                signed_sidecar_blob_hash: hash('5'),
                payload_blob_hash: hash('6'),
            },
        }
    }

    #[test]
    fn source_and_subject_expose_complete_typed_cas_edges() {
        let source = source();
        let source_value = source.to_value().unwrap();
        let links = crate::object_closure::object_links(&source_value).unwrap();
        assert_eq!(links.object_hashes, vec![hash('4')]);
        assert_eq!(
            links.blob_hashes,
            vec![
                hash('2'),
                hash('3'),
                hash('5'),
                hash('6'),
                hash('b'),
                hash('e')
            ]
        );
        assert_eq!(
            source.digest().unwrap(),
            crate::objects::canonical_value_digest(&source_value).unwrap()
        );

        let subject = GuestRuntimeMaterializationSubject {
            schema: GUEST_RUNTIME_MATERIALIZATION_SCHEMA,
            kind: GUEST_RUNTIME_MATERIALIZATION_SUBJECT_KIND.to_owned(),
            coordinate_digest: hash('7'),
            runtime_manifest_hash: hash('8'),
            source_evidence_hash: source.digest().unwrap(),
        };
        let links = crate::object_closure::object_links(&subject.to_value().unwrap()).unwrap();
        assert_eq!(
            links.object_hashes,
            vec![hash('8'), source.digest().unwrap()]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        );
        assert!(links.blob_hashes.is_empty());
    }

    #[test]
    fn source_refuses_missing_manifest_and_noncanonical_order() {
        let mut missing = source();
        missing.signed_bundle_manifests.clear();
        assert!(missing.validate().is_err());
        let mut unordered = source();
        unordered
            .signed_recipe_items
            .push(MaterializationSignedItem {
                resolved_ref: "config:codex/earlier".to_owned(),
                ..unordered.signed_recipe_items[0].clone()
            });
        assert!(unordered.validate().is_err());
        let mut invalid_hash = source();
        invalid_hash.executor.item_source_object_hash = "not-a-hash".to_owned();
        assert!(invalid_hash.validate().is_err());
        let mut uppercase = source();
        uppercase.executor.payload_blob_hash = "A".repeat(64);
        assert!(uppercase.validate().is_err());
        let mut contradictory = source();
        contradictory
            .signed_recipe_items
            .push(MaterializationSignedItem {
                signed_blob_hash: hash('9'),
                ..contradictory.signed_recipe_items[0].clone()
            });
        assert!(contradictory.validate().is_err());
    }
}
