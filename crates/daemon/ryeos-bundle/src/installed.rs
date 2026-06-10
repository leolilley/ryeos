//! Verified installed bundle registration loader.
//!
//! This mirrors the daemon bootstrap semantics for `.ai/node/bundles/*.yaml`
//! without depending on `ryeos-app`: registrations must be signed, trusted,
//! structured YAML records in the `bundles` section; paths are canonicalized;
//! symlinks and collisions fail closed.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ryeos_engine::contracts::{SignatureEnvelope, TrustClass};
use ryeos_engine::trust::TrustStore;
use serde::Deserialize;

use crate::manifest::{derive_provides_kinds, parse_manifest};
use crate::plan::{BundleSource, PlanInput};

#[derive(Debug, Clone)]
pub struct InstalledBundleRecord {
    pub name: String,
    pub registration_path: PathBuf,
    pub bundle_root: PathBuf,
}

impl InstalledBundleRecord {
    pub fn into_plan_input(self) -> PlanInput {
        PlanInput {
            name: self.name,
            source: BundleSource::Installed {
                registration_path: self.registration_path,
                bundle_root: self.bundle_root,
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleRegistrationBody {
    #[serde(default)]
    kind: Option<String>,
    section: String,
    #[serde(default)]
    id: Option<String>,
    path: PathBuf,
    #[allow(dead_code)]
    #[serde(default)]
    command_registration_caps: Vec<String>,
}

/// Load installed bundles from signed node bundle registrations.
pub fn load_installed_bundle_records(app_root: &Path) -> Result<Vec<InstalledBundleRecord>> {
    let operator_config_root =
        ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).config();
    let trust_store = TrustStore::load(None, &operator_config_root)
        .context("installed bundles: load operator trust store")?;
    load_installed_bundle_records_with_trust(app_root, &trust_store)
}

pub fn load_installed_bundle_records_with_trust(
    app_root: &Path,
    trust_store: &TrustStore,
) -> Result<Vec<InstalledBundleRecord>> {
    let bundles_dir = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("bundles");
    if !bundles_dir.is_dir() {
        return Ok(Vec::new());
    }

    let envelope = yaml_signature_envelope();
    let mut records = Vec::new();

    for entry in fs::read_dir(&bundles_dir)
        .with_context(|| format!("failed to read node bundles dir {}", bundles_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let file_type = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to stat {}", path.display()))?
            .file_type();
        if file_type.is_symlink() || !file_type.is_file() {
            bail!(
                "node bundle registration at {} is not a regular file (symlinks rejected)",
                path.display()
            );
        }

        let ext = path.extension().and_then(|ext| ext.to_str());
        if ext != Some("yaml") && ext != Some("yml") {
            continue;
        }

        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("node bundle registration has no filename stem")?
            .to_string();

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        verify_signed_trusted_yaml(&content, &path, trust_store, &envelope)
            .with_context(|| format!("verify node bundle registration {}", path.display()))?;

        let body: BundleRegistrationBody = serde_yaml::from_str(&strip_signature(&content))
            .with_context(|| format!("failed to parse YAML body of {}", path.display()))?;

        if body.kind.as_deref().is_some_and(|kind| kind != "node") {
            bail!(
                "node bundle registration {} declares kind {:?}, expected 'node'",
                path.display(),
                body.kind
            );
        }
        if body.section != "bundles" {
            bail!(
                "node bundle registration {} declares section '{}', expected 'bundles'",
                path.display(),
                body.section
            );
        }
        if body.id.as_deref().is_some_and(|id| id != name) {
            bail!(
                "node bundle registration {} declares id {:?}, expected filename id '{}'",
                path.display(),
                body.id,
                name
            );
        }
        if !body.path.is_absolute() {
            bail!(
                "bundle '{}' path must be absolute, got {} in {}",
                name,
                body.path.display(),
                path.display()
            );
        }
        if !body.path.is_dir() {
            bail!(
                "bundle '{}' path '{}' does not exist or is not a directory (declared in {})",
                name,
                body.path.display(),
                path.display()
            );
        }

        let canonical = body.path.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize bundle '{}' path '{}'",
                name,
                body.path.display()
            )
        })?;

        verify_installed_manifest(&name, &canonical, trust_store)
            .with_context(|| format!("verify installed bundle '{}' manifest", name))?;

        records.push(InstalledBundleRecord {
            name,
            registration_path: path,
            bundle_root: canonical,
        });
    }

    records.sort_by(|a, b| a.name.cmp(&b.name));
    check_collisions(&records)?;
    Ok(records)
}

pub fn load_installed_plan_inputs(app_root: &Path) -> Result<Vec<PlanInput>> {
    load_installed_bundle_records(app_root).map(|records| {
        records
            .into_iter()
            .map(InstalledBundleRecord::into_plan_input)
            .collect()
    })
}

fn verify_installed_manifest(
    name: &str,
    bundle_root: &Path,
    trust_store: &TrustStore,
) -> Result<()> {
    let manifest_path = bundle_root.join(ryeos_engine::AI_DIR).join("manifest.yaml");
    let file_type = fs::symlink_metadata(&manifest_path)
        .with_context(|| format!("failed to stat {}", manifest_path.display()))?
        .file_type();
    if file_type.is_symlink() || !file_type.is_file() {
        bail!(
            "installed bundle '{}' at {} has no regular signed .ai/manifest.yaml (symlinks rejected)",
            name,
            bundle_root.display()
        );
    }

    let envelope = yaml_signature_envelope();
    let content = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    verify_signed_trusted_yaml(&content, &manifest_path, trust_store, &envelope)?;
    let manifest = parse_manifest(bundle_root, name)?
        .with_context(|| format!("installed bundle '{}' manifest is missing", name))?;
    let mut expected_provides = derive_provides_kinds(&bundle_root.join(ryeos_engine::AI_DIR))?;
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

fn verify_signed_trusted_yaml(
    content: &str,
    path: &Path,
    trust_store: &TrustStore,
    envelope: &SignatureEnvelope,
) -> Result<()> {
    let header = ryeos_engine::item_resolution::parse_signature_header(content, envelope)
        .context(format!("{} has no valid signature line", path.display()))?;
    let (trust_class, _) =
        ryeos_engine::trust::verify_item_signature(content, &header, envelope, trust_store)
            .with_context(|| format!("signature verification failed for {}", path.display()))?;
    if trust_class != TrustClass::Trusted {
        bail!(
            "{} is not trusted (trust_class: {:?}); only trusted registrations are allowed",
            path.display(),
            trust_class
        );
    }
    Ok(())
}

fn yaml_signature_envelope() -> SignatureEnvelope {
    SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    }
}

fn strip_signature(content: &str) -> String {
    content
        .lines()
        .skip_while(|line| line.starts_with("# ryeos:signed:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_start()
        .to_string()
}

fn check_collisions(records: &[InstalledBundleRecord]) -> Result<()> {
    let mut by_name: HashMap<&str, &InstalledBundleRecord> = HashMap::new();
    let mut by_path: HashMap<&Path, &InstalledBundleRecord> = HashMap::new();

    for record in records {
        if let Some(prev) = by_name.get(record.name.as_str()) {
            bail!(
                "node config section 'bundles' has duplicate name '{}': first registered from '{}', second from '{}'",
                record.name,
                prev.registration_path.display(),
                record.registration_path.display()
            );
        }
        if let Some(prev) = by_path.get(record.bundle_root.as_path()) {
            bail!(
                "node config section 'bundles' has duplicate canonical path '{}': first registered as '{}' (from {}), second as '{}' (from {})",
                record.bundle_root.display(),
                prev.name,
                prev.registration_path.display(),
                record.name,
                record.registration_path.display()
            );
        }
        by_name.insert(record.name.as_str(), record);
        by_path.insert(record.bundle_root.as_path(), record);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use rand::rngs::OsRng;

    struct Layout {
        _tmp: tempfile::TempDir,
        system: PathBuf,
        user: PathBuf,
        key: SigningKey,
    }

    impl Layout {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let system = tmp.path().join("system");
            let user = tmp.path().join("user");
            let trust_dir = user.join(".ai/config/keys/trusted");
            fs::create_dir_all(&trust_dir).unwrap();
            let key = SigningKey::generate(&mut OsRng);
            ryeos_engine::trust::pin_key(&key.verifying_key(), "test", &trust_dir, None).unwrap();
            Self {
                _tmp: tmp,
                system,
                user,
                key,
            }
        }

        fn trust_store(&self) -> TrustStore {
            TrustStore::load(None, &self.user.join(".ai/config")).unwrap()
        }

        fn write_bundle(&self, name: &str) -> PathBuf {
            let bundle = self.system.join(".ai/bundles").join(name);
            fs::create_dir_all(bundle.join(".ai")).unwrap();
            let manifest = format!(
                "name: {name}\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds: []\nuses_kinds: []\n"
            );
            let signed = lillux::signature::sign_content(&manifest, &self.key, "#", None);
            fs::write(bundle.join(".ai/manifest.yaml"), signed).unwrap();
            bundle
        }

        fn write_registration(&self, name: &str, bundle: &Path) -> PathBuf {
            let dir = self.system.join(".ai/node/bundles");
            fs::create_dir_all(&dir).unwrap();
            let body = format!(
                "kind: node\nsection: bundles\nid: {name}\npath: {}\n",
                bundle.display()
            );
            let signed = lillux::signature::sign_content(&body, &self.key, "#", None);
            let path = dir.join(format!("{name}.yaml"));
            fs::write(&path, signed).unwrap();
            path
        }
    }

    #[test]
    fn loads_signed_registered_installed_bundle() {
        let layout = Layout::new();
        let bundle = layout.write_bundle("core");
        let reg = layout.write_registration("core", &bundle);

        let records =
            load_installed_bundle_records_with_trust(&layout.system, &layout.trust_store())
                .unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "core");
        assert_eq!(records[0].registration_path, reg);
        assert_eq!(records[0].bundle_root, bundle.canonicalize().unwrap());
    }

    #[test]
    fn rejects_unsigned_registration() {
        let layout = Layout::new();
        let bundle = layout.write_bundle("core");
        let dir = layout.system.join(".ai/node/bundles");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("core.yaml"),
            format!(
                "kind: node\nsection: bundles\nid: core\npath: {}\n",
                bundle.display()
            ),
        )
        .unwrap();

        let err = load_installed_bundle_records_with_trust(&layout.system, &layout.trust_store())
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("signature"), "expected signature error: {msg}");
    }

    #[test]
    fn rejects_id_filename_mismatch() {
        let layout = Layout::new();
        let bundle = layout.write_bundle("core");
        let dir = layout.system.join(".ai/node/bundles");
        fs::create_dir_all(&dir).unwrap();
        let body = format!(
            "kind: node\nsection: bundles\nid: wrong\npath: {}\n",
            bundle.display()
        );
        let signed = lillux::signature::sign_content(&body, &layout.key, "#", None);
        fs::write(dir.join("core.yaml"), signed).unwrap();

        let err = load_installed_bundle_records_with_trust(&layout.system, &layout.trust_store())
            .unwrap_err();
        assert!(err.to_string().contains("expected filename id"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_installed_manifest() {
        let layout = Layout::new();
        let bundle = layout.write_bundle("core");
        layout.write_registration("core", &bundle);
        let manifest = bundle.join(".ai/manifest.yaml");
        let target = bundle.join(".ai/manifest-real.yaml");
        fs::rename(&manifest, &target).unwrap();
        std::os::unix::fs::symlink(&target, &manifest).unwrap();

        let err = load_installed_bundle_records_with_trust(&layout.system, &layout.trust_store())
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("regular signed"),
            "expected regular-file rejection: {msg}"
        );
    }

    #[test]
    fn rejects_installed_manifest_provides_mismatch() {
        let layout = Layout::new();
        let bundle = layout.write_bundle("core");
        layout.write_registration("core", &bundle);
        let schema_dir = bundle.join(".ai/node/engine/kinds/extra");
        fs::create_dir_all(&schema_dir).unwrap();
        fs::write(
            schema_dir.join("extra.kind-schema.yaml"),
            "kind: config\ndirectory: extra\nextensions: []\n",
        )
        .unwrap();

        let err = load_installed_bundle_records_with_trust(&layout.system, &layout.trust_store())
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("provides_kinds mismatch"),
            "expected provides mismatch: {msg}"
        );
    }
}
