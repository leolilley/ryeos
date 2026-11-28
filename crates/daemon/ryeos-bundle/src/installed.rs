//! The planner's manifest step over bundles the node has already registered.
//!
//! Registrations themselves are read and validated in one place, the node
//! runtime's `BundleBootstrapLoader`. This module takes the validated
//! registrations it yields and checks each installed bundle's signed manifest
//! against the tree on disk before it becomes a planner input.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ryeos_engine::trust::TrustStore;

use crate::manifest::{derive_provides_kinds, load_verified_manifest};
use crate::plan::{BundleSource, PlanInput};

/// A registration the node's validator has already accepted.
#[derive(Debug, Clone)]
pub struct RegisteredBundle {
    pub name: String,
    pub registration_path: PathBuf,
    /// The canonical bundle root.
    pub bundle_root: PathBuf,
}

/// Planner inputs for validated registrations.
///
/// For each, the bundle's `.ai/manifest.yaml` must be a regular, signed,
/// trusted file whose `provides_kinds` equals the kinds the tree provides.
pub fn plan_inputs_from_registrations(
    registrations: &[RegisteredBundle],
    trust_store: &TrustStore,
) -> Result<Vec<PlanInput>> {
    let mut inputs = Vec::with_capacity(registrations.len());
    for registration in registrations {
        verify_installed_manifest(&registration.name, &registration.bundle_root, trust_store)
            .with_context(|| format!("verify installed bundle '{}' manifest", registration.name))?;
        inputs.push(PlanInput {
            name: registration.name.clone(),
            source: BundleSource::Installed {
                registration_path: registration.registration_path.clone(),
                bundle_root: registration.bundle_root.clone(),
            },
        });
    }
    Ok(inputs)
}

fn verify_installed_manifest(
    name: &str,
    bundle_root: &Path,
    trust_store: &TrustStore,
) -> Result<()> {
    let ai_dir = bundle_root.join(ryeos_engine::AI_DIR);
    let manifest = load_verified_manifest(&ai_dir, name, trust_store)?.manifest;
    let mut expected_provides = derive_provides_kinds(&ai_dir)?;
    let mut claimed_provides = manifest.provides_kinds.clone();
    expected_provides.sort();
    claimed_provides.sort();
    if claimed_provides != expected_provides {
        bail!(
            "installed bundle '{}' manifest provides_kinds mismatch: manifest declares {:?}, disk provides {:?}",
            name,
            claimed_provides,
            expected_provides
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use rand::rngs::OsRng;
    use std::fs;

    struct Layout {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        key: SigningKey,
        trust: TrustStore,
    }

    impl Layout {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().to_path_buf();
            let user = root.join("user");
            fs::create_dir_all(&user).unwrap();
            let key = SigningKey::generate(&mut OsRng);
            ryeos_engine::test_support::pin_signer(
                &user,
                &key.verifying_key(),
                "test",
                ryeos_engine::trust::SignerRole::Operator,
                ryeos_engine::trust::SignerScope::everywhere(),
            );
            let trust = TrustStore::load_node_registry(&user).unwrap();
            Self {
                _tmp: tmp,
                root,
                key,
                trust,
            }
        }

        fn write_bundle(&self, name: &str) -> RegisteredBundle {
            let bundle = self.root.join("bundles").join(name);
            fs::create_dir_all(bundle.join(".ai")).unwrap();
            let manifest = format!(
                "name: {name}\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds: []\nuses_kinds: []\n"
            );
            let signed = lillux::signature::sign_content(&manifest, &self.key, "#", None);
            fs::write(bundle.join(".ai/manifest.yaml"), signed).unwrap();
            RegisteredBundle {
                name: name.to_string(),
                registration_path: self.root.join(format!("{name}.yaml")),
                bundle_root: bundle.canonicalize().unwrap(),
            }
        }
    }

    #[test]
    fn a_signed_manifest_becomes_an_installed_plan_input() {
        let layout = Layout::new();
        let core = layout.write_bundle("core");
        let inputs =
            plan_inputs_from_registrations(std::slice::from_ref(&core), &layout.trust).unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].name, "core");
        assert_eq!(inputs[0].source.root_path(), &core.bundle_root);
    }

    #[test]
    fn an_unsigned_manifest_is_refused() {
        let layout = Layout::new();
        let core = layout.write_bundle("core");
        fs::write(
            core.bundle_root.join(".ai/manifest.yaml"),
            "name: core\nversion: '1.0'\nprovides_kinds: []\n",
        )
        .unwrap();
        let error = plan_inputs_from_registrations(&[core], &layout.trust).unwrap_err();
        assert!(format!("{error:#}").contains("signature"), "{error:#}");
    }

    #[test]
    fn a_symlinked_manifest_is_refused() {
        let layout = Layout::new();
        let core = layout.write_bundle("core");
        let manifest = core.bundle_root.join(".ai/manifest.yaml");
        let target = core.bundle_root.join(".ai/manifest-real.yaml");
        fs::rename(&manifest, &target).unwrap();
        lillux::tree_entry::test_support::make_symlink(&target, &manifest).unwrap();
        let error = plan_inputs_from_registrations(&[core], &layout.trust).unwrap_err();
        assert!(format!("{error:#}").contains("regular"), "{error:#}");
    }

    #[test]
    fn a_manifest_that_misdeclares_the_provided_kinds_is_refused() {
        let layout = Layout::new();
        let core = layout.write_bundle("core");
        let schema_dir = core.bundle_root.join(".ai/node/engine/kinds/extra");
        fs::create_dir_all(&schema_dir).unwrap();
        fs::write(
            schema_dir.join("extra.kind-schema.yaml"),
            "kind: config\ndirectory: extra\nextensions: []\n",
        )
        .unwrap();
        let error = plan_inputs_from_registrations(&[core], &layout.trust).unwrap_err();
        assert!(
            format!("{error:#}").contains("provides_kinds mismatch"),
            "{error:#}"
        );
    }
}
