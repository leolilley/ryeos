//! CAS ownership of one historically signed lifecycle adapter generation.
//!
//! This object retains bytes for operation-bound cleanup after an installed
//! Bundle is replaced. It is not a current publisher grant, adapter admission,
//! or worker qualification. The reader must verify every retained signature
//! and join the object to a protected operation before using it.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const RETAINED_LIFECYCLE_ARTIFACTS_KIND: &str = "retained_lifecycle_artifacts";
pub const RETAINED_LIFECYCLE_ARTIFACTS_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetainedLifecycleExecutableRole {
    Adapter,
    Supervisor,
    Launcher,
    RestorationVerifier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetainedLifecycleSpecRole {
    Provider,
    SnapshotProduction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedLifecycleExecutable {
    pub role: RetainedLifecycleExecutableRole,
    pub item_ref: String,
    pub target_triple: String,
    pub payload_blob_hash: String,
    pub payload_bytes: u64,
    pub mode: u32,
    pub signed_manifest_ref_blob_hash: String,
    pub manifest_object_blob_hash: String,
    pub item_source_object_hash: String,
    pub signed_sidecar_blob_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedLifecycleSpec {
    pub role: RetainedLifecycleSpecRole,
    pub path: String,
    pub blob_hash: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedLifecycleArtifacts {
    pub schema: u32,
    pub kind: String,
    pub bundle_name: String,
    pub signed_bundle_manifest_blob_hash: String,
    pub bundle_manifest_body_digest: String,
    pub signer_fingerprint: String,
    /// Historical public verifier only. Its presence never restores a
    /// withdrawn grant for a new provider operation.
    pub signer_verifying_key: String,
    pub declaration_id: String,
    pub executables: Vec<RetainedLifecycleExecutable>,
    pub specs: Vec<RetainedLifecycleSpec>,
}

impl RetainedLifecycleArtifacts {
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
        anyhow::ensure!(
            self.schema == RETAINED_LIFECYCLE_ARTIFACTS_SCHEMA
                && self.kind == RETAINED_LIFECYCLE_ARTIFACTS_KIND,
            "retained lifecycle artifact schema/kind is not current"
        );
        require_segment("Bundle name", &self.bundle_name)?;
        require_segment("lifecycle declaration", &self.declaration_id)?;
        for (label, hash) in [
            (
                "signed Bundle manifest",
                &self.signed_bundle_manifest_blob_hash,
            ),
            ("Bundle manifest body", &self.bundle_manifest_body_digest),
            ("Bundle signer", &self.signer_fingerprint),
        ] {
            require_hash(label, hash)?;
        }
        let encoded = self
            .signer_verifying_key
            .strip_prefix("ed25519:")
            .ok_or_else(|| anyhow::anyhow!("historical Bundle verifier is not Ed25519"))?;
        let key_bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(encoded)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("historical Bundle verifier has wrong length"))?;
        let verifier = lillux::crypto::VerifyingKey::from_bytes(&key_bytes)?;
        anyhow::ensure!(
            !verifier.is_weak()
                && base64::engine::general_purpose::STANDARD.encode(key_bytes) == encoded
                && lillux::crypto::fingerprint(&verifier) == self.signer_fingerprint,
            "historical Bundle verifier differs from signer"
        );
        let roles = self
            .executables
            .iter()
            .map(|item| item.role)
            .collect::<Vec<_>>();
        anyhow::ensure!(
            roles
                == [
                    RetainedLifecycleExecutableRole::Adapter,
                    RetainedLifecycleExecutableRole::Supervisor,
                    RetainedLifecycleExecutableRole::Launcher,
                ]
                || roles
                    == [
                        RetainedLifecycleExecutableRole::Adapter,
                        RetainedLifecycleExecutableRole::Supervisor,
                        RetainedLifecycleExecutableRole::Launcher,
                        RetainedLifecycleExecutableRole::RestorationVerifier,
                    ],
            "retained lifecycle executable roles are incomplete or out of order"
        );
        for item in &self.executables {
            anyhow::ensure!(
                item.signed_manifest_ref_blob_hash
                    == self.executables[0].signed_manifest_ref_blob_hash
                    && item.manifest_object_blob_hash
                        == self.executables[0].manifest_object_blob_hash,
                "retained lifecycle executable roles do not share one signed executor manifest"
            );
            require_target(&item.target_triple)?;
            let prefix = format!("bin/{}/", item.target_triple);
            let name = item.item_ref.strip_prefix(&prefix).ok_or_else(|| {
                anyhow::anyhow!("retained lifecycle executable ref differs from target")
            })?;
            require_segment("lifecycle executable", name)?;
            anyhow::ensure!(
                (1..=1024 * 1024 * 1024).contains(&item.payload_bytes),
                "retained lifecycle executable byte bound is invalid"
            );
            anyhow::ensure!(
                item.mode <= 0o777 && item.mode & 0o111 != 0,
                "retained lifecycle executable mode is not executable"
            );
            for (label, hash) in [
                ("lifecycle executable payload", &item.payload_blob_hash),
                (
                    "signed executor manifest ref",
                    &item.signed_manifest_ref_blob_hash,
                ),
                ("executor manifest object", &item.manifest_object_blob_hash),
                ("executor ItemSource", &item.item_source_object_hash),
                ("signed executor sidecar", &item.signed_sidecar_blob_hash),
            ] {
                require_hash(label, hash)?;
            }
        }
        let spec_roles = self.specs.iter().map(|item| item.role).collect::<Vec<_>>();
        anyhow::ensure!(
            spec_roles == [RetainedLifecycleSpecRole::Provider]
                || spec_roles
                    == [
                        RetainedLifecycleSpecRole::Provider,
                        RetainedLifecycleSpecRole::SnapshotProduction,
                    ],
            "retained lifecycle spec roles are incomplete or out of order"
        );
        for spec in &self.specs {
            anyhow::ensure!(
                spec.path.len() <= 256
                    && !spec.path.starts_with('/')
                    && spec.path.split('/').all(|part| {
                        !part.is_empty() && part != "." && part != ".." && part.len() <= 128
                    }),
                "retained lifecycle spec path is invalid"
            );
            require_hash("retained lifecycle spec", &spec.blob_hash)?;
            anyhow::ensure!(
                (1..=1024 * 1024).contains(&spec.bytes),
                "retained lifecycle spec byte bound is invalid"
            );
        }
        Ok(())
    }
}

fn require_hash(label: &str, hash: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} is not a lowercase SHA-256 digest"
    );
    Ok(())
}

fn require_segment(label: &str, value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
        "{label} is not a bounded segment"
    );
    Ok(())
}

fn require_target(value: &str) -> anyhow::Result<()> {
    require_segment("lifecycle executable target", value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn fixture() -> RetainedLifecycleArtifacts {
        let verifier = lillux::crypto::SigningKey::from_bytes(&[23u8; 32]).verifying_key();
        let target = "x86_64-unknown-linux-gnu";
        let executable = |role, name: &str, item_hash: char| RetainedLifecycleExecutable {
            role,
            item_ref: format!("bin/{target}/{name}"),
            target_triple: target.to_owned(),
            payload_blob_hash: hash('a'),
            payload_bytes: 128,
            mode: 0o755,
            signed_manifest_ref_blob_hash: hash('b'),
            manifest_object_blob_hash: hash('c'),
            item_source_object_hash: hash(item_hash),
            signed_sidecar_blob_hash: hash('e'),
        };
        RetainedLifecycleArtifacts {
            schema: RETAINED_LIFECYCLE_ARTIFACTS_SCHEMA,
            kind: RETAINED_LIFECYCLE_ARTIFACTS_KIND.to_owned(),
            bundle_name: "render-sandbox".to_owned(),
            signed_bundle_manifest_blob_hash: hash('f'),
            bundle_manifest_body_digest: hash('a'),
            signer_fingerprint: lillux::crypto::fingerprint(&verifier),
            signer_verifying_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(verifier.to_bytes())
            ),
            declaration_id: "render-sandbox-early-access".to_owned(),
            executables: vec![
                executable(RetainedLifecycleExecutableRole::Adapter, "adapter", 'a'),
                executable(
                    RetainedLifecycleExecutableRole::Supervisor,
                    "supervisor",
                    'b',
                ),
                executable(RetainedLifecycleExecutableRole::Launcher, "launcher", 'c'),
            ],
            specs: vec![RetainedLifecycleSpec {
                role: RetainedLifecycleSpecRole::Provider,
                path: "specs/provider-spec.json".to_owned(),
                blob_hash: hash('b'),
                bytes: 37,
            }],
        }
    }

    #[test]
    fn exact_lifecycle_closure_has_typed_edges() {
        let object = fixture();
        let value = object.to_value().unwrap();
        assert_eq!(
            RetainedLifecycleArtifacts::from_value(&value).unwrap(),
            object
        );
        let links = crate::object_closure::object_links(&value).unwrap();
        assert_eq!(links.object_hashes, vec![hash('a'), hash('b'), hash('c')]);
        assert_eq!(
            links.blob_hashes,
            vec![hash('a'), hash('b'), hash('c'), hash('e'), hash('f')]
        );
    }

    #[test]
    fn lifecycle_closure_rejects_missing_roles_and_foreign_verifier() {
        let mut object = fixture();
        object.executables.remove(1);
        assert!(object.validate().is_err());
        let mut object = fixture();
        object.signer_fingerprint = hash('c');
        assert!(object.validate().is_err());
        let mut object = fixture();
        object.specs.clear();
        assert!(object.validate().is_err());
        let mut object = fixture();
        object.executables[1].manifest_object_blob_hash = hash('d');
        assert!(object.validate().is_err());
    }
}
